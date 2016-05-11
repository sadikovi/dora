use libc;

use ctxt::{Context, FctId, FctKind, get_ctxt};
use execstate::ExecState;
use mem::ptr::Ptr;
use stack::Stacktrace;

pub use self::param::*;
pub use self::reg::*;

pub mod emit;
pub mod instr;
pub mod param;
pub mod reg;
pub mod trap;

pub fn get_rootset(ctxt: &Context) -> Vec<usize> {
    let mut rootset = Vec::new();
    let mut pc : usize = 0;
    unsafe { asm!("lea (%rip), $0": "=r"(pc)) }

    let mut fp : usize = 0;
    unsafe { asm!("mov %rbp, $0": "=r"(fp)) }

    determine_rootset(&mut rootset, ctxt, fp, pc);

    while fp != 0 {
        pc = unsafe { *((fp + 8) as *const usize) };
        determine_rootset(&mut rootset, ctxt, fp, pc);

        fp = unsafe { *(fp as *const usize) };
    }

    rootset
}

fn determine_rootset(rootset: &mut Vec<usize>, ctxt: &Context, fp: usize, pc: usize) {
    let code_map = ctxt.code_map.lock().unwrap();
    let fct_id = code_map.get(pc);

    if let Some(fct_id) = fct_id {
        let mut lineno = 0;

        ctxt.fct_by_id(fct_id, |fct| {
            if let FctKind::Source(ref src) = fct.kind {
                if let Some(ref jit_fct) = src.jit_fct {
                    let offset = pc - (jit_fct.fct_ptr().raw() as usize);
                    let gcpoint = jit_fct.gcpoint_for_offset(offset as i32);

                    if let Some(gcpoint) = jit_fct.gcpoint_for_offset(offset as i32) {
                        println!("gcpoint = {:?}", gcpoint);

                        for offset in &gcpoint.offsets {
                            let addr = (fp as isize + *offset as isize) as usize;
                            let obj = unsafe { *(addr as *const usize) };

                            println!("addr={:x} obj={:x}", addr, obj);
                            rootset.push(obj);
                        }

                    } else {
                        panic!("no gc point found");
                    }
                }
            }
        });
    }
}

pub fn get_stacktrace(ctxt: &Context, es: &ExecState) -> Stacktrace {
    let mut stacktrace = Stacktrace::new();
    determine_stack_entry(&mut stacktrace, ctxt, es.pc);

    let mut rbp = es.regs[reg::RBP.int() as usize];

    while rbp != 0 {
        let ra = unsafe { *((rbp + 8) as *const usize) };
        determine_stack_entry(&mut stacktrace, ctxt, ra);

        rbp = unsafe { *(rbp as *const usize) };
    }

    return stacktrace;
}

fn determine_stack_entry(stacktrace: &mut Stacktrace, ctxt: &Context, pc: usize) {
    let code_map = ctxt.code_map.lock().unwrap();
    let fct_id = code_map.get(pc);

    if let Some(fct_id) = fct_id {
        let mut lineno = 0;

        ctxt.fct_by_id(fct_id, |fct| {
            if let FctKind::Source(ref src) = fct.kind {
                let jit_fct = src.jit_fct.as_ref().unwrap();
                let offset = pc - (jit_fct.fct_ptr().raw() as usize);
                lineno = jit_fct.lineno_for_offset(offset as i32);

                if lineno == 0 {
                    panic!("lineno not found for program point");
                }
            }

        });

        stacktrace.push_entry(fct_id, lineno);
    }
}
