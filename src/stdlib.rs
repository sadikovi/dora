use std::ffi::CStr;
use std::io::{self, Write};
use std::os::raw::c_char;

use libc;

use ctxt::{Context, get_ctxt};
use object::Str;


pub extern "C" fn assert(val: bool) {
    if !val {
        unsafe {
            libc::_exit(1);
        }
    }
}

pub extern "C" fn print(val: Str) {
    unsafe {
        let buf = CStr::from_ptr(val.data() as *const c_char);
        io::stdout().write(buf.to_bytes()).unwrap();
    };
}

pub extern "C" fn println(val: Str) {
    print(val);
    println!("");
}

pub extern "C" fn strcmp(lhs: Str, rhs: Str) -> i32 {
    unsafe {
        libc::strcmp(lhs.data() as *const i8, rhs.data() as *const i8)
    }
}

pub extern "C" fn strcat(lhs: Str, rhs: Str) -> Str {
    let ctxt = get_ctxt();
    let mut gc = ctxt.gc.lock().unwrap();

    Str::concat(&mut gc, lhs, rhs)
}
