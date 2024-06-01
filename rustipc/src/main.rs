extern crate libc;
extern crate uuid;
extern crate scoped_tls;
extern crate anyhow;
use std::os::raw::{c_void,c_char};
use std::{mem,ptr,process,slice,thread};
use uuid::Uuid;
use std::sync::{Arc, Mutex};
use std::any::Any;
use std::ffi::CString;
use std::collections::HashMap;
use anyhow::{anyhow,Error};
use std::ffi::CStr;
use std::time::Instant;

const IPC_PORT_PATH_MAX: usize = 256;
const NO_ERROR:i32 = 0;
const ERR_TIMED_OUT:i32=-3;
const DEFAULT_IPC_CONNECT_WARN_TIMEOUT:i32=1000;

pub struct HandleT{
    refcnt: Arc<Mutex<u32>>,
    flags:u32,
    wait_event:* mut Event,
    cookie:Option<Box<dyn Any>>,
    use_async:bool,
} 

struct Handle {
    handle: Arc<HandleT>, // 持有 Arc 版本的句柄
}
    
impl Handle {
    fn new(new_handle: *mut HandleT) ->  Result<Handle, Error>  {
        if new_handle.is_null() {
            return Err (anyhow!("is none")); // 如果传入的指针为空，返回 None
        }
        let handle = unsafe { Arc::from_raw(new_handle) }; // 安全地从裸指针创建 Arc
        {
            let mut refcnt = handle.refcnt.lock().unwrap();
            *refcnt += 1;
        }
        Ok(Handle { handle })
    }
}
    
impl Drop for Handle {
    fn drop(&mut self) {
        let mut refcnt = self.handle.refcnt.lock().unwrap();
        if *refcnt == 1 {
            // 如果引用计数为 1，说明没有其他引用，可以清理资源
        } else {
            *refcnt -= 1; // 否则减少引用计数
        }
    }
}

#[repr(u32)]
enum IpcConnectState{
    IpcConnectWaitForPort = 0x1,
    IpcConnectAsync = 0x2,
    IpcConnectMask = IpcConnectState::IpcConnectWaitForPort as u32 | IpcConnectState::IpcConnectAsync as u32,
}

#[repr(u32)]
enum IpcHandleState{
    IpcHandlePollNone    = 0x0,
    IpcHandlePollReady   = 0x1,
    IpcHandlePollError   = 0x2,
    IpcHandlePollHup     = 0x4,
    IpcHandlePollMsg     = 0x8,
    IpcHandlePollSendUnblocked = 0x10,
}

enum MyError {
    InvalidArgs,
    ErrNoMemory,
}

struct  Event{

}

pub struct HandleRef {
    parent: *mut HandleT,
    handle:*mut HandleT,
    id: u32,
    emask: u32,
    cookie: *mut c_void,
}

struct IpcPort {
    path: [c_char; IPC_PORT_PATH_MAX],  //字符数组，用于存储IPC端口的路径
    uuid: *const Uuid,
    state:IpcState,
    flags:usize,
    num_recv_bufs:usize,
    recv_buf_size:*const i8,
    handle:HandleT,
}
    
type UctxT = u32;

#[derive(PartialEq)]  //自动生成实现了 PartialEq trait 的代码,可以用于比较两个值是否相等
enum IpcState{
    IpcPortStateInvalid		= 0,
    IpcPortStateListening	= 1,
}

#[repr(C)]
pub struct HandleIdT{
    value:usize,   
}

pub fn port_create(path: *const c_char, num_recv_bufs:u32,recv_buf_size:u32, flags:u32)->Result<usize, Error>{
    let ctx: * mut UctxT =current_uctx();
    let uuid:  Uuid= Uuid::new_v5(&Uuid::NAMESPACE_OID, b"q-pid");
    let mut port_handle: * mut HandleT=ptr::null_mut();
    let handle_id = HandleIdT {
        value:23,
    };
    let mut tmp_path:[c_char; IPC_PORT_PATH_MAX] = [0; IPC_PORT_PATH_MAX];
    // 从用户空间复制路径
    let ret=strlcpy_from_user(tmp_path.as_mut_ptr() ,path,IPC_PORT_PATH_MAX);   //将用户空间的字符串path拷贝到内核空间的缓冲区tmp_path中，并返回拷贝的字节数。
    if ret<0  {return Err(anyhow!("Buffer overflow"));}
    if  ret as usize >=mem::size_of_val(&tmp_path){ return  Err(anyhow!("already exists"));}  
    // 创建新的端口
    let ret = ipc_port_create(&uuid, tmp_path.as_ptr() as *const i8, num_recv_bufs, recv_buf_size, flags, &mut port_handle);
    if ret != NO_ERROR {
        return Err(anyhow!("ipc_port_create failed with error: {}", ret));
    }
    // 将句柄安装到用户上下文中
    let ret=port_install(ctx,port_handle, &handle_id);
    if ret != NO_ERROR {
        Handle::new(port_handle)?;
    }
    //发布以正常运行
    let ret=ipc_port_publish(port_handle);
    if ret != NO_ERROR  {  //在指定的端口上，向设备发送指定路径的数据，并成功返回NO_ERROR
        uctx_handle_remove(ctx, &handle_id, ptr::null_mut());
        return Err(anyhow!("Failed to publish IPC port"));
    }
    return Ok(unsafe { mem::transmute(handle_id) });
}
pub fn current_uctx() -> *mut UctxT {
    let current_thread_id = thread::current().id();
    let tls_key = format!("uctx_tls_{:?}", current_thread_id); // 生成的 tls_key 字符串将用作哈希表 tls 的键，用于存储和检索与当前线程相关的用户上下文的指针。
    unsafe {
        // 尝试从当前线程的 TLS 变量中获取上下文指针
        let mut uctx_ptr = ptr::null_mut();
        let mut tls:HashMap<_,_> = Default::default();
        if let Some(ptr) = tls.get(&tls_key).map(|p| *p){  //将获取到的指针解引用，并将其复制一份返回
            uctx_ptr = ptr;
        }
        if uctx_ptr==ptr::null_mut() {
            // 如果当前线程没有上下文，则创建一个新的上下文
            let mut uctx: UctxT = 0; // 这里假设上下文类型为 u32，并初始化为 0
            // 调用 uctx_create 函数创建新的上下文
            uctx_create(&mut uctx as *mut UctxT);
            // 将上下文指针保存到当前线程的 TLS 变量中
            tls.insert(tls_key, uctx as *mut UctxT);
            // 返回新创建的上下文指针
            uctx_ptr = uctx as *mut UctxT;
        }
    
        uctx_ptr
    }
}
pub fn accept(handle_id:HandleIdT,user_uuid:* mut c_void) ->Result<u64, Error>{
    let ctx:* mut UctxT=current_uctx();
    
    let phandle: * mut  HandleT=ptr::null_mut() ;
    let chandle: * mut HandleT=ptr::null_mut();
    let ret :u64;
    let new_id= HandleIdT {
        value:42,
    };
    let uuid : Uuid=Uuid::new_v5(&Uuid::NAMESPACE_OID, b"m-pid");
    let peer_uuid_ptr: &Uuid = &uuid;
    let ret=|ctx:* mut UctxT, handle_id:HandleIdT, mut handle_p:* mut HandleT|-> Result<i32, Error>{  
        let handle_t = Arc::new(HandleT {
            refcnt: Arc::new(Mutex::new(1)),
            flags: 0,
            wait_event: ptr::null_mut(), // 假设没有事件在等待
            cookie: None, // 假设没有 cookie
            use_async: false,
        });
        let handle_t_clone  = Arc::clone(&handle_t);
        let handle_ptr = Arc::as_ptr(&handle_t) as * mut HandleT;
        let tmp_ref= HandleRef {
            parent:handle_ptr,
            handle:handle_ptr,
            id: 1,           // 假设 id 是 1
            emask: 0,        // 假设事件掩码 emask 是 0
            cookie: ptr::null_mut() as *mut c_void,
        };
        let ret=unsafe{uctx_handle_get_tmp_ref(ctx, handle_id, &tmp_ref)} ;  //获取一个临时的上下文句柄引用
	    if ret == NO_ERROR {
		    handle_p = tmp_ref.handle;
	    }
	    return Ok(ret);
    };
    let result=ret (ctx, handle_id,  phandle);
    match result {
        Err(_) => {
            // 处理错误情况
            println!("An error occurred.");
        }
        Ok(value) if value != NO_ERROR => {
            // 处理非 NO_ERROR 的情况
            println!("Result is not NO_ERROR, got: {}", value);
        }
        Ok(_) => {
            // 处理 NO_ERROR 的情况
            println!("The handle with ID 23 has been found ");
        }
    }
    let ret = unsafe{ipc_port_accept(phandle, chandle, * peer_uuid_ptr)};
    if ret != NO_ERROR {
        let handle = Handle::new(phandle)?;
        return  Ok(ret.try_into().unwrap());
    }
    let ret = unsafe{uctx_handle_install(ctx, chandle, &new_id)};
    if ret != NO_ERROR {
        let handle = Handle::new(chandle)?;
    }
    unsafe{return Ok(mem::transmute(new_id))};
}
pub fn connect(path:*const i8, flags:u32)-> Result<u64,  Error>{
    let uuid:  Uuid=Uuid::new_v5(&Uuid::NAMESPACE_OID, b"r-pid");
    let ctx:* mut UctxT=current_uctx();
	let mut chandle:* mut HandleT =ptr::null_mut();
	let mut tmp_path:[c_char; IPC_PORT_PATH_MAX] = [0; IPC_PORT_PATH_MAX];
	let mut ret:i32 = 0;
	let handle_id= HandleIdT {
        value:23,
    };
	if (flags & IpcConnectState::IpcConnectMask as u32) !=0 {
	/* unsupported flags specified */
		return Err(anyhow!("invalid arguments"));
	}
	ret =strlcpy_from_user(tmp_path.as_mut_ptr(), path , IPC_PORT_PATH_MAX);
	if ret < 0{
        return Err(anyhow!("Buffer overflow"));
    }
	if (ret as usize) >= mem::size_of_val(&tmp_path){
        return  Err(anyhow!("invalid arguments"));
    }
	let ret = unsafe{ipc_port_connect_async(&uuid,tmp_path.as_mut_ptr(), mem::size_of_val(& tmp_path).try_into().unwrap(),flags, chandle)};
	if ret != NO_ERROR{
        return  Ok(ret as u64);
    }
    if (flags & IpcConnectState::IpcConnectAsync as u32)!=0 {
		let event:usize = 0;
		let timeout_msecs = DEFAULT_IPC_CONNECT_WARN_TIMEOUT;
        let infinitetime:i32 =1000;
		let ret = unsafe{handle_wait(chandle, event, timeout_msecs)};
		if ret == ERR_TIMED_OUT {
			let ret = unsafe{handle_wait(chandle, event, infinitetime)};
        }
		if ret < 0 {
			/* timeout or other error */
			let handle = Handle::new(chandle)?;
			return Ok(ret.try_into().unwrap());
		}

		if ((event as u32 & IpcHandleState::IpcHandlePollHup as u32)!=0) &&
		    ((event as u32 & IpcHandleState::IpcHandlePollMsg as u32))!=0 {
			/* hangup and no pending messages */
			let handle = Handle::new(chandle)?;
			return Err(anyhow!("channel closed"));
		}
		if (event as u32 & IpcHandleState::IpcHandlePollReady as u32)!=0 {
			/* not connected */
			let handle = Handle::new(chandle)?;
			return Err(anyhow!("not ready"));
		}
    }
    let ret = unsafe{uctx_handle_install(ctx, chandle, &handle_id)};
	if ret != NO_ERROR {
	/* Failed to install handle into user context */
		let handle = Handle::new(chandle)?;
		return Ok(ret.try_into().unwrap());
	}
	unsafe{return Ok(mem::transmute(handle_id))};
}   
fn strlcpy_from_user(tmp_path: * mut i8, path: * const i8, max_len: usize) ->i32{
    let len = unsafe { str_len(path) };
    let path_slice = unsafe{slice::from_raw_parts(path, len + 1)}; // +1 为了包括null终止符
    let tmp_slice = unsafe{slice::from_raw_parts_mut(tmp_path, len + 1)};
    // 复制字符串
    tmp_slice[..len].copy_from_slice(&path_slice[..len]);
    // 添加终止符
    if len < max_len {
        tmp_slice[len] = 0;
        return len.try_into().unwrap();
    } else {
        return -1;
    }
}
    
unsafe fn str_len(s: *const i8) -> usize {
    if s.is_null() {
        return 0;
    }

    let mut len = 0;
    while *s.add(len) != 0 {
        len += 1;
    }
    len
}

pub fn ipc_port_create(uuid: &Uuid, path: *const i8, num_recv_bufs: u32, recv_buf_size: u32, flags: u32, port_handle: *mut *mut HandleT) -> i32 {
    NO_ERROR
}
pub fn uctx_create(ctx: *mut u32)->u32{
    0
}
pub fn port_install(ctx: *mut UctxT, port_handle: *mut HandleT, handle_id: &HandleIdT) -> i32 {
    NO_ERROR
}
pub fn ipc_port_publish(port_handle: *mut HandleT) -> i32 {
    NO_ERROR
}
pub fn uctx_handle_remove(ctx: *mut UctxT, handle_id: &HandleIdT, _port_handle: *mut HandleT) -> i32{
    NO_ERROR
}
pub fn handle_wait(handle:* mut HandleT, handle_event:usize, timeout:i32)->i32{
    NO_ERROR
}
pub fn ipc_port_connect_async(cid:  &Uuid,path: *const i8,max_path: u32,flags: u32,chandle_ptr: *mut HandleT) -> i32{
    NO_ERROR
}
pub fn ipc_port_accept(phandle: *mut HandleT, chandle_ptr:  *mut HandleT, uuid_ptr:Uuid) -> i32{
    NO_ERROR
}
pub fn uctx_handle_get_tmp_ref(ctx:* mut UctxT,handle_id:HandleIdT,out:* const HandleRef)->i32{
    NO_ERROR
}
pub fn uctx_handle_install(ctx:* mut UctxT,handle:* mut HandleT,id:* const HandleIdT)->i32{
    NO_ERROR
}

#[allow(warnings)]
fn main()  {
    let start = Instant::now();
    let path = CStr::from_bytes_with_nul(b"/tmp/ipc\0").unwrap();
    match port_create(path.as_ptr(), 10, 1024, 0) {
        Ok(handle_id) => {
            println!("Port created with handle ID: {}", handle_id);
            let user_uuid: *mut c_void = std::ptr::null_mut();
            match accept(HandleIdT { value: handle_id }, user_uuid) {
                Ok(new_id) => {
                    println!("Channel created with handle ID:{}",new_id);
                }
                Err(e) => println!("Error: {}", e),
            }
        }
        Err(e) => println!("Error: {}", e),
    }
    let flags = 0;
    match connect(path.as_ptr(), flags) {
        Ok(handle_id) => println!("Connected successfully with handle ID: {}", handle_id),
        Err(e) => println!("Error: {}", e),
    }
    let end = Instant::now();
    println!("代码运行时间: {:?}", end.duration_since(start));
}
