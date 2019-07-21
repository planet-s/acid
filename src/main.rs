//!Acid testing program
#![feature(thread_local, asm)]

fn e<T, E: ToString>(error: Result<T, E>) -> Result<T, String> {
    error.map_err(|e| e.to_string())
}

fn create_test() -> Result<(), String> {
    use std::fs;
    use std::io::{self, Read, Write};
    use std::path::PathBuf;

    let mut test_dir = PathBuf::new();
    test_dir.push("test_dir");

    let mut test_file = test_dir.clone();
    test_file.push("test_file");
    let test_file_err = fs::File::create(&test_file).err().map(|err| err.kind());
    if test_file_err != Some(io::ErrorKind::NotFound) {
        return Err(format!("Incorrect open error: {:?}, should be NotFound", test_file_err));
    }

    fs::create_dir(&test_dir).map_err(|err| format!("{}", err))?;

    let test_data = "Test data";
    {
        let mut file = fs::File::create(&test_file).map_err(|err| format!("{}", err))?;
        file.write(test_data.as_bytes()).map_err(|err| format!("{}", err))?;
    }

    {
        let mut file = fs::File::open(&test_file).map_err(|err| format!("{}", err))?;
        let mut buffer: Vec<u8> = Vec::new();
        file.read_to_end(&mut buffer).map_err(|err| format!("{}", err))?;
        assert_eq!(buffer.len(), test_data.len());
        for (&a, b) in buffer.iter().zip(test_data.bytes()) {
            if a != b {
                return Err(format!("{} did not contain the correct data", test_file.display()));
            }
        }
    }

    Ok(())
}

fn page_fault_test() -> Result<(), String> {
    use std::thread;

    thread::spawn(|| {
        println!("{:X}", unsafe { *(0xDEADC0DE as *const u8) });
    }).join().unwrap();

    Ok(())
}

pub fn ptrace() -> Result<(), String> {
    use std::{
        fs::File,
        io,
        mem,
        os::unix::io::{AsRawFd, FromRawFd, RawFd}
    };
    use strace::*;

    let pid = e(unsafe { syscall::clone(0) })?;
    if pid == 0 {
        extern "C" fn sighandler(_: usize) {
            unsafe {
                asm!("
                    mov rax, 158 // SYS_YIELD
                    syscall
                "
                : : : : "intel", "volatile");
            }
        }
        extern "C" fn sigreturn() {
            unsafe {
                asm!("
                    mov rax, 119 // SYS_SIGRETURN
                    syscall
                    ud2
                "
                : : : : "intel", "volatile");
            }
        }
        unsafe {
            asm!("
                // Push any arguments from rust to the stack so we're
                // free to use whatever registers we want
                push $1
                push $0
                mov rbp, rsp

                // Wait until tracer is started
                mov rax, 20 // SYS_GETPID
                syscall

                mov rdi, rax

                mov rax, 37 // SYS_KILL
                mov rsi, 19 // SIGSTOP
                syscall

                // Start of body:

                // Test basic singlestepping
                mov rax, 1
                push rax
                mov rax, 2
                push rax
                mov rax, 3
                pop rax
                pop rax

                // Test memory access
                push 3
                push 2
                push 1
                add rsp, 8*3 // pop 3 items, ignore values

                // Testing floating point
                push 32
                fild QWORD PTR [rsp]
                fsqrt
                add rsp, 8 // pop 1 item, ignore value

                // Make sure event is raised when child forks
                mov rax, 120 // SYS_CLONE
                xor rdi, rdi
                syscall
                test rax, rax
                je exit

                // Wait for child process, to make sure an ignored process is continued
                mov rdi, rax
                mov rax, 7
                push 0
                mov rsi, rsp
                xor rdx, rdx
                syscall
                add rsp, 8

                // Another fork attempt, but test what happens when not ignored
                mov rax, 120 // SYS_CLONE
                xor rdi, rdi
                syscall
                test rax, rax
                je exit

                // Test behavior of signals
                mov rax, 67 // SYS_SIGACTION
                mov rdi, 10 // SIGUSR1
                push 0 // sa_flags
                push 0 // sa_mask[1]
                push 0 // sa_mask[0]
                push [rbp] // sa_handler
                mov rsi, rsp
                xor rdx, rdx
                mov r10, [rbp+0x8]
                syscall
                add rsp, 8*4

                mov rax, 20 // SYS_GETPID
                syscall

                mov rdi, rax
                mov rax, 37 // SYS_KILL
                mov rsi, 10 // SIGUSR1
                syscall

                // aaand again
                mov rax, 37 // SYS_KILL
                syscall

                // Test behavior if tracer aborts a breakpoint before it's reached
                call wait_for_a_while

                mov rax, 158 // SYS_YIELD
                syscall

                mov rax, 20 // SYS_GETPID
                syscall

                mov rdi, rax
                mov rax, 37 // SYS_KILL
                mov rsi, 19 // SIGSTOP
                syscall

                // Test nonblock & sysemu
                call wait_for_a_while

                exit:
                mov rax, 20 // SYS_GETPID
                syscall

                mov rdi, rax
                mov rax, 1 // SYS_EXIT
                syscall
                ud2

                // Without a jump, this code is unreachable. Therefore function definitions go here.

                wait_for_a_while:
                mov rax, 4294967295
                wait_for_a_while_loop:
                sub rax, 1
                jne wait_for_a_while_loop
                ret
                "
                : // no outputs
                : "r"(sighandler as usize), "r"(sigreturn as usize)
                : // no clobbers
                : "intel", "volatile"
            );
        }
    }

    println!("My PID: {}", e(syscall::getpid())?);
    println!("Waiting until child (pid {}) is ready to be traced...", pid);
    let mut status = 0;
    e(syscall::waitpid(pid, &mut status, syscall::WUNTRACED))?;

    println!("Done! Attaching tracer...");

    // Stop and attach process + get handle to registers. This also
    // tests the behavior of dup(...)
    let proc_file = e(File::open(format!("proc:{}/trace", pid)))?;
    let regs_file = unsafe {
        File::from_raw_fd(e(syscall::dup(proc_file.as_raw_fd() as usize, b"regs/int"))? as RawFd)
    };
    let regs_file_float = unsafe {
        File::from_raw_fd(e(syscall::dup(regs_file.as_raw_fd() as usize, b"regs/float"))? as RawFd)
    };

    let mut tracer = Tracer {
        file: proc_file,
        regs: Registers {
            float: regs_file_float,
            int: regs_file
        },
        mem: e(Memory::attach(pid))?
    };

    println!("Schedule restart of process when resumed...");
    e(syscall::kill(pid, syscall::SIGCONT))?;

    println!("Stepping away from the syscall instruction...");
    e(tracer.next(Stop::INSTRUCTION))?;

    println!("Testing basic singlestepping...");
    assert_eq!(e(e(tracer.next(Stop::INSTRUCTION))?.regs.get_int())?.rax, 1);
    assert_eq!(e(e(tracer.next(Stop::INSTRUCTION))?.regs.get_int())?.rax, 2);
    assert_eq!(e(e(tracer.next(Stop::INSTRUCTION))?.regs.get_int())?.rax, 2);
    assert_eq!(e(e(tracer.next(Stop::INSTRUCTION))?.regs.get_int())?.rax, 3);
    assert_eq!(e(e(tracer.next(Stop::INSTRUCTION))?.regs.get_int())?.rax, 2);
    assert_eq!(e(e(tracer.next(Stop::INSTRUCTION))?.regs.get_int())?.rax, 1);

    println!("Testing memory access...");
    e(tracer.next(Stop::INSTRUCTION))?;
    e(tracer.next(Stop::INSTRUCTION))?;

    e(tracer.next(Stop::INSTRUCTION))?;
    let regs = e(tracer.regs.get_int())?;

    unsafe {
        union Stack {
            words: [usize; 3],
            bytes: [u8; 3 * mem::size_of::<usize>()]
        }
        let mut out = Stack { words: [0; 3] };
        e(tracer.mem.read(regs.rsp as *const _, &mut out.bytes))?;
        assert_eq!(out.words, [1, 2, 3]);
        assert_eq!(e(tracer.mem.cursor())? as usize, regs.rsp + out.bytes.len());
    }

    e(tracer.next(Stop::INSTRUCTION))?;

    println!("Testing floating point...");
    for _ in 0..3 {
        e(tracer.next(Stop::INSTRUCTION))?;
    }
    let regs = e(tracer.regs.get_float())?;
    let f = regs.st_space_nth(0);
    let fs = regs.st_space();
    assert_eq!(fs, [f, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
    assert!((f - 5.65685424949238).abs() < std::f64::EPSILON);

    println!("Testing fork event");
    assert!(e(tracer.next_event(Stop::SYSCALL))?.is_none()); // pre-syscall
    let mut handler = e(tracer.next_event(Stop::SYSCALL))?.ok_or("Syscall completed without yielding fork event")?; // post-syscall
    let events = e(handler.iter().collect::<io::Result<Vec<_>>>())?;

    assert_eq!(events.len(), 1);
    match events[0] {
        PtraceEvent::Clone(pid) => println!("Obtained fork (PID {})", pid),
        ref e => return Err(format!("Wrong event type: {:?}", e))
    }
    assert!(e(handler.retry())?.is_none());

    println!("Testing fork event - but actually handling the fork");
    for _ in 0..3 { // pre-post-waitpid, pre-clone
        assert!(e(tracer.next_event(Stop::SYSCALL))?.is_none());
    }
    let mut handler = e(tracer.next_event(Stop::SYSCALL))?.ok_or("Syscall completed without yielding fork event")?;
    // handler = e(handler.retry())?.ok_or("Program completed without yielding fork event")?;
    let events = e(handler.iter().collect::<io::Result<Vec<_>>>())?;
    assert_eq!(events.len(), 1);
    match events[0] {
        PtraceEvent::Clone(pid) => {
            let mut child = e(Tracer::attach(pid))?;
            println!("-> Fork attached (PID {})", pid);

            assert_eq!(e(e(child.next(Stop::SYSCALL))?.regs.get_int())?.rax, syscall::SYS_GETPID);
            e(child.next(Stop::SYSCALL))?;
            println!("-> Fork executed GETPID");

            assert_eq!(e(e(child.next(Stop::SYSCALL))?.regs.get_int())?.rax, syscall::SYS_EXIT);
            assert_eq!(child.next(Stop::COMPLETION).unwrap_err().raw_os_error(), Some(syscall::ESRCH));
            println!("-> Fork executed EXIT");
        },
        ref e => return Err(format!("Wrong event type: {:?}", e))
    }
    assert!(e(handler.retry())?.is_none());

    println!("Testing signals");
    assert_eq!(e(e(tracer.next(Stop::SYSCALL))?.regs.get_int())?.rax, syscall::SYS_SIGACTION);
    e(tracer.next(Stop::SYSCALL))?; // post-syscall sigaction
    assert_eq!(e(e(tracer.next(Stop::SYSCALL))?.regs.get_int())?.rax, syscall::SYS_GETPID);
    e(tracer.next(Stop::SYSCALL))?; // post-syscall getpid
    assert_eq!(e(e(tracer.next(Stop::SYSCALL))?.regs.get_int())?.rax, syscall::SYS_KILL);
    // kill doesn't return *yet*

    let mut handler = e(tracer.next_event(Stop::SYSCALL))?.ok_or("Syscall completed without yielding  event")?;
    let events = e(handler.iter().collect::<io::Result<Vec<_>>>())?;

    assert_eq!(events.len(), 1);
    match events[0] {
        PtraceEvent::Signal(signal) => {
            assert_eq!(signal, syscall::SIGUSR1);
            println!("Obtained signal");
        },
        ref e => return Err(format!("Wrong event type: {:?}", e))
    }

    assert!(e(handler.retry())?.is_none());
    for i in 0..2 {
        assert_eq!(e(tracer.regs.get_int())?.rax, syscall::SYS_YIELD);
        e(tracer.next(Stop::SYSCALL))?; // post-syscall yield
        assert_eq!(e(e(tracer.next(Stop::SYSCALL))?.regs.get_int())?.rax, syscall::SYS_SIGRETURN);
        // sigreturn doesn't return
        e(tracer.next(Stop::SYSCALL))?; // post-syscall kill!

        if i == 0 {
            e(tracer.next(Stop::SIGNAL))?;
            assert_eq!(e(tracer.regs.get_int())?.rax, syscall::SYS_KILL);
            e(tracer.next(Stop::SYSCALL))?;
        }
    }

    // Activate nonblock
    let mut tracer = e(tracer.nonblocking())?;

    println!("Testing behavior of obsolete breakpoints...");
    e(tracer.next(Stop::SYSCALL))?;
    e(tracer.next(Stop::COMPLETION))?;
    println!("Tracee RAX: {}", e(tracer.regs.get_int())?.rax);

    println!("Waiting for next signal from tracee that it's ready to be traced again...");
    e(syscall::waitpid(pid, &mut status, syscall::WUNTRACED))?;

    println!("Setting sysemu breakpoint...");
    e(tracer.next(Stop::SYSCALL | Stop::SYSEMU))?;

    println!("Schedule restart of process after breakpoint is set...");
    e(syscall::kill(pid, syscall::SIGCONT))?;

    println!("After non-blocking ptrace, execution continues as normal:");
    for _ in 0..5 {
        println!("Tracee RAX: {}", e(tracer.regs.get_int())?.rax);
    }

    println!("Waiting... Five times... To make sure it doesn't get stuck forever...");
    for _ in 0..5 {
        e(tracer.wait())?;
    }

    println!("Overriding GETPID call...");
    let mut regs = e(tracer.regs.get_int())?;
    assert_eq!(regs.rax, syscall::SYS_GETPID);
    regs.rax = 123;
    e(tracer.regs.set_int(&regs))?;

    let mut tracer = e(tracer.blocking())?;

    println!("Checking exit syscall...");
    e(tracer.next(Stop::SYSCALL))?;
    let regs = e(tracer.regs.get_int())?;
    assert_eq!(regs.rax, syscall::SYS_EXIT);
    assert_eq!(regs.rdi, 123);
    assert_eq!(tracer.next(Stop::SYSCALL).unwrap_err().raw_os_error(), Some(syscall::ESRCH));

    println!("Checking exit status (waitpid nohang)...");
    let mut status = 0;
    e(syscall::waitpid(pid, &mut status, syscall::WNOHANG))?;
    assert!(syscall::wifexited(status));
    assert_eq!(syscall::wexitstatus(status), 123);

    println!("Trying to do illegal things...");
    for id in 0..=1_000_000 {
        let err = File::open(format!("proc:{}/regs/int", id)).map(|_| None).unwrap_or_else(|err| err.raw_os_error());
        assert!(
            err == Some(syscall::EPERM) || err == Some(syscall::ESRCH),
            "The cops ignored that I tried to illegally open PID {}: {:?}", id, err
        );
    }

    println!("All done and tested!");

    Ok(())
}

fn switch_test() -> Result<(), String> {
    use std::thread;
    use x86::time::rdtscp;

    let tsc = unsafe { rdtscp() };

    let switch_thread = thread::spawn(|| -> usize {
        let mut j = 0;
        while j < 500 {
            thread::yield_now();
            j += 1;
        }
        j
    });

    let mut i = 0;
    while i < 500 {
        thread::yield_now();
        i += 1;
    }

    let j = switch_thread.join().unwrap();

    let dtsc = unsafe { rdtscp() } - tsc;
    println!("P {} C {} T {}", i, j, dtsc);

    Ok(())
}

fn tcp_fin_test() -> Result<(), String> {
    use std::io::Write;
    use std::net::TcpStream;

    let mut conn = TcpStream::connect("static.redox-os.org:80").map_err(|err| format!("{}", err))?;
    conn.write(b"TEST").map_err(|err| format!("{}", err))?;
    drop(conn);

    Ok(())
}

fn thread_test() -> Result<(), String> {
    use std::process::Command;
    use std::thread;
    use std::time::Instant;

    println!("Trying to stop kernel...");

    let start = Instant::now();
    while start.elapsed().as_secs() == 0 {}

    println!("Kernel preempted!");

    println!("Trying to kill kernel...");

    let mut threads = Vec::new();
    for i in 0..10 {
        threads.push(thread::spawn(move || {
            let mut sub_threads = Vec::new();
            for j in 0..10 {
                sub_threads.push(thread::spawn(move || {
                    Command::new("ion")
                        .arg("-c")
                        .arg(&format!("echo {}:{}", i, j))
                        .spawn().unwrap()
                        .wait().unwrap();
                }));
            }

            Command::new("ion")
                .arg("-c")
                .arg(&format!("echo {}", i))
                .spawn().unwrap()
                .wait().unwrap();

            for sub_thread in sub_threads {
                let _ = sub_thread.join();
            }
        }));
    }

    for thread in threads {
        let _ = thread.join();
    }

    println!("Kernel survived thread test!");

    Ok(())
}

/// Test of zero values in thread BSS
#[thread_local]
static mut TBSS_TEST_ZERO: usize = 0;
/// Test of non-zero values in thread data.
#[thread_local]
static mut TDATA_TEST_NONZERO: usize = 0xFFFFFFFFFFFFFFFF;

fn tls_test() -> Result<(), String> {
    use std::thread;

    thread::spawn(|| {
        unsafe {
            assert_eq!(TBSS_TEST_ZERO, 0);
            TBSS_TEST_ZERO += 1;
            assert_eq!(TBSS_TEST_ZERO, 1);
            assert_eq!(TDATA_TEST_NONZERO, 0xFFFFFFFFFFFFFFFF);
            TDATA_TEST_NONZERO -= 1;
            assert_eq!(TDATA_TEST_NONZERO, 0xFFFFFFFFFFFFFFFE);
        }
    }).join().unwrap();

    unsafe {
        assert_eq!(TBSS_TEST_ZERO, 0);
        TBSS_TEST_ZERO += 1;
        assert_eq!(TBSS_TEST_ZERO, 1);
        assert_eq!(TDATA_TEST_NONZERO, 0xFFFFFFFFFFFFFFFF);
        TDATA_TEST_NONZERO -= 1;
        assert_eq!(TDATA_TEST_NONZERO, 0xFFFFFFFFFFFFFFFE);
    }

    Ok(())
}

fn main() {
    use std::collections::BTreeMap;
    use std::{env, process};
    use std::time::Instant;

    let mut tests: BTreeMap<&'static str, fn() -> Result<(), String>> = BTreeMap::new();
    tests.insert("create_test", create_test);
    tests.insert("page_fault", page_fault_test);
    tests.insert("ptrace", ptrace);
    tests.insert("switch", switch_test);
    tests.insert("tcp_fin", tcp_fin_test);
    tests.insert("thread", thread_test);
    tests.insert("tls", tls_test);

    let mut ran_test = false;
    for arg in env::args().skip(1) {
        if let Some(test) = tests.get(&arg.as_str()) {
            ran_test = true;

            let time = Instant::now();
            let res = test();
            let elapsed = time.elapsed();
            match res {
                Ok(_) => {
                    println!("acid: {}: passed: {} ns", arg, elapsed.as_secs() * 1000000000 + elapsed.subsec_nanos() as u64);
                },
                Err(err) => {
                    println!("acid: {}: failed: {}", arg, err);
                }
            }
        } else {
            println!("acid: {}: not found", arg);
            process::exit(1);
        }
    }

    if ! ran_test {
        for test in tests {
            println!("{}", test.0);
        }
    }
}
