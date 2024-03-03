use crate::memory::{split_protected, MemoryOp};
use crate::{Error, Tracee};

use nix::sys::ptrace;
use procfs::process::MMPermissions;
use std::ffi::c_void;
use std::marker::PhantomData;

const WORD_SIZE: usize = std::mem::size_of::<usize>();

/// Write operations don't have any unique properties at this time.
/// If needed, later this can be replaced with `struct WriteOp(MemoryOp, <extra props>)`.
type WriteOp = MemoryOp;

/// Allows to write data to different locations in debuggee's memory as a single operation.
/// This implementation can select different strategies for different memory pages.
pub struct WriteMemory<'a> {
    tracee: &'a Tracee,
    write_ops: Vec<WriteOp>,
    /// We need only an immutable reference because we don't rewrite values of variables in `WriteOp`.
    _marker: PhantomData<&'a ()>,
}

impl<'a> WriteMemory<'a> {
    pub(crate) fn new(tracee: &'a Tracee) -> Self {
        WriteMemory {
            tracee,
            write_ops: Vec::new(),
            _marker: PhantomData,
        }
    }

    /// Writes a value of type `T` into debuggee's memory at location `remote_base`.
    /// The value will be read from the provided variable `val`.
    /// You should call `apply` in order to execute the memory write operation.
    /// The lifetime of the variable `val` is bound to the lifetime of `WriteMemory`.
    pub fn write<T: ?Sized>(mut self, val: &'a T, remote_base: usize) -> Self {
        WriteOp::split_on_page_boundary(
            &WriteOp {
                remote_base,
                local_ptr: val as *const T as *mut u8,
                local_len: std::mem::size_of_val(val),
            },
            &mut self.write_ops,
        );
        self
    }

    /// Writes a slice of type `T` into debuggee's memory at location `remote_base`.
    /// The entries will be read from the provided slice `val`.
    /// You should call `apply` in order to execute the memory write operation.
    /// The lifetime of the variable `val` is bound to the lifetime of `WriteMemory`.
    pub fn write_slice<T>(mut self, val: &'a [T], remote_base: usize) -> Self {
        WriteOp::split_on_page_boundary(
            &WriteOp {
                remote_base,
                local_ptr: val.as_ptr() as *mut u8,
                local_len: val.len() * std::mem::size_of::<T>(),
            },
            &mut self.write_ops,
        );
        self
    }

    /// Executes the memory write operation.
    ///
    /// # Remote safety
    ///
    /// It's a user's responsibility to ensure that debuggee memory addresses are valid.
    /// This function only reads memory from the local process.
    pub fn apply(self) -> Result<(), Error> {
        let protected_maps: Vec<_> = self
            .tracee
            .memory_maps()?
            .into_iter()
            .filter(|map| !map.perms.contains(MMPermissions::WRITE))
            .collect();

        let (protected, writable) = split_protected(&protected_maps, &self.write_ops);

        // Break write operations into word groups.
        let protected_groups = protected.into_iter().flat_map(|op| op.into_word_sized_ops());

        if !writable.is_empty() {
            self.write_process_vm(&writable)?;
        }
        self.write_ptrace(protected_groups)?;

        Ok(())
    }

    /// Executes memory writing operations using ptrace only.
    /// This function should be used only for testing purposes.
    #[cfg(test)]
    unsafe fn apply_ptrace(self) -> Result<(), Error> {
        self.write_ptrace(self.write_ops.iter().flat_map(|op| op.into_word_sized_ops()))
    }

    /// Allows to write data to different locations in debuggee's memory as a single operation.
    /// It requires a memory page to be writable.
    fn write_process_vm(&self, write_ops: &[WriteOp]) -> Result<usize, Error> {
        let pid = self.tracee.pid;
        let bytes_expected = write_ops.iter().fold(0, |sum, read_op| sum + read_op.local_len);

        if bytes_expected > isize::MAX as usize {
            panic!("Write size too big");
        };

        // Create a list of `IoVec`s and remote `IoVec`s
        let remote: Vec<_> = write_ops.iter().map(|read_op| read_op.as_remote_iovec()).collect();
        let local: Vec<_> = write_ops.iter().map(|read_op| read_op.as_local()).collect();

        let bytes_read = nix::sys::uio::process_vm_writev(pid, &local, &remote)?;
        if bytes_read != bytes_expected {
            return Err(Error::IncompleteRead { req: bytes_expected, read: bytes_read });
        }

        Ok(bytes_read)
    }

    /// Allows to write to write-protected pages.
    /// On Linux, this will result in multiple system calls and it's inefficient.
    fn write_ptrace(&self, write_ops: impl Iterator<Item = WriteOp>) -> Result<(), Error> {
        let pid = self.tracee.pid;
        for op in write_ops {
            assert!(op.local_len <= WORD_SIZE);

            if op.local_len < WORD_SIZE {
                // Write op is smaller than a single word, so we should read memory before rewriting it.
                let mut word = ptrace::read(pid, op.remote_base as *mut c_void)?.to_ne_bytes();
                let src_bytes: &[u8] =
                    unsafe { std::slice::from_raw_parts(op.local_ptr as *const u8, op.local_len) };

                word[..op.local_len].clone_from_slice(&src_bytes[..op.local_len]);

                unsafe {
                    ptrace::write(
                        pid,
                        op.remote_base as *mut c_void,
                        usize::from_ne_bytes(word) as *mut usize as *mut c_void,
                    )?;
                }
            } else {
                unsafe {
                    let word = std::ptr::read(op.local_ptr as *const usize);
                    ptrace::write(pid, op.remote_base as *mut c_void, word as *mut c_void)?;
                }
            }
        }

        Ok(())
    }
}

/// Breaks the memory write operation into groups of words suitable for writing
/// with `ptrace::write`.
///
/// ptrace(PTRACE_POKETEXT) can write only a single word (usize) to the destination address.
/// So if we want to write e.g. 1 byte, we need to read 8 bytes at the destination address
/// first, replace the first byte, and overwrite it at the destination address again.
/// Obviously, this is very inefficient since it requires a lot of context switches,
/// but sometimes it's the only way to overwrite the target's memory.
struct WordSizedOps {
    mem_op: WriteOp,
}

impl WriteOp {
    /// Converts this memory operation into an iterator that returns word-sized memory operations.
    /// This is required for ptrace which is not capable of writing data larger than a single word
    /// (which is equal to usize - or 8 bytes - on x86_64).
    fn into_word_sized_ops(self) -> WordSizedOps {
        WordSizedOps { mem_op: self }
    }
}

impl Iterator for WordSizedOps {
    type Item = WriteOp;

    /// Produces a next word for writing to debuggee's memory.
    ///
    /// # Safety
    ///
    /// This function doesn't guarantee safety of produced pointers.
    /// It's a user's responsibility to ensure the validity of provided memory addresses and sizes.
    fn next(&mut self) -> Option<MemoryOp> {
        if self.mem_op.local_len == 0 {
            return None;
        }

        let group_size = std::cmp::min(WORD_SIZE, self.mem_op.local_len);

        let output = WriteOp {
            remote_base: self.mem_op.remote_base,
            local_ptr: self.mem_op.local_ptr,
            local_len: group_size,
        };

        self.mem_op.local_len -= group_size;
        self.mem_op.local_ptr = unsafe { self.mem_op.local_ptr.add(group_size) };
        self.mem_op.remote_base += group_size;

        Some(output)
    }
}

// #[cfg(test)]
// mod tests {
//     use super::{WriteMemory, WriteOp};
//     use crate::memory::PAGE_SIZE;
//     use crate::Debugger;
// 
//     use nix::sys::mman::{mprotect, ProtFlags};
//     use nix::sys::ptrace;
//     use nix::sys::signal::{self, Signal};
//     use nix::sys::wait;
//     use nix::unistd::{fork, ForkResult};
// 
//     use std::alloc::{alloc_zeroed, dealloc, Layout};
//     use std::ptr;
// 
//     #[test]
//     fn write_memory_proc_vm() {
//         let var: usize = 52;
//         let var2: u8 = 128;
// 
//         let write_var_op: usize = 0;
//         let write_var2_op: u8 = 0;
// 
//         let mut debugger = Debugger::<&str>::me();
//         debugger.view(nix::unistd::getpid()).unwrap();
//         let mut debugger = debugger.lock();
//         let process = debugger.processes().next().expect("No processes");
// 
//         WriteMemory::new(&process)
//             .write(&var, &write_var_op as *const _ as usize)
//             .write(&var2, &write_var2_op as *const _ as usize)
//             .apply()
//             .expect("Failed to write memory");
// 
//         unsafe {
//             assert_eq!(ptr::read_volatile(&write_var_op), var);
//             assert_eq!(ptr::read_volatile(&write_var2_op), var2);
//         }
//     }
// 
//     #[test]
//     fn write_memory_ptrace() {
//         let var: usize = 52;
//         let var2: u8 = 128;
//         let dyn_array = vec![1, 2, 3, 4];
// 
//         let write_var_op: usize = 0;
//         let write_var2_op: u8 = 0;
//         let write_array = [0u8; 4];
// 
//         match unsafe { fork() } {
//             Ok(ForkResult::Child) => {
//                 ptrace::traceme().unwrap();
// 
//                 unsafe {
//                     assert_eq!(ptr::read_volatile(&write_var_op), 0);
//                     assert_eq!(ptr::read_volatile(&write_var2_op), 0);
//                 }
// 
//                 // Wait for the parent process to signal to continue.
//                 signal::raise(Signal::SIGSTOP).unwrap();
// 
//                 // Catch the panic so that we can report back to the original process.
//                 let test_res = std::panic::catch_unwind(|| unsafe {
//                     assert_eq!(ptr::read_volatile(&write_var_op), var);
//                     assert_eq!(ptr::read_volatile(&write_var2_op), var2);
//                     assert_eq!(&ptr::read_volatile(&write_array), dyn_array.as_slice());
//                 });
// 
//                 // Return an explicit status code.
//                 std::process::exit(if test_res.is_ok() { 0 } else { 100 });
//             }
//             Ok(ForkResult::Parent { child, .. }) => {
//                 // Wait for child.
//                 let wait_status = wait::waitpid(child, None).unwrap();
// 
//                 match wait_status {
//                     wait::WaitStatus::Stopped(_pid, _sig) => {}
//                     status => {
//                         signal::kill(child, Signal::SIGKILL).unwrap();
//                         panic!("Unexpected child status: {:?}", status);
//                     }
//                 }
// 
//                 let mut debugger = Debugger::<&str>::me();
//                 debugger.view(child).unwrap();
//                 let mut debugger = debugger.lock();
//                 let process = debugger.processes().next().expect("No processes");
// 
//                 // Write memory to the child's process
//                 unsafe {
//                     WriteMemory::new(process)
//                         .write(&var, &write_var_op as *const _ as usize)
//                         .write(&var2, &write_var2_op as *const _ as usize)
//                         .write_slice(&dyn_array, write_array.as_ptr() as usize)
//                         .apply_ptrace()
//                         .expect("Failed to write memory");
//                 }
// 
//                 ptrace::detach(child, Some(Signal::SIGCONT)).unwrap();
// 
//                 // Check if the child assertions are successful.
//                 let exit_status = wait::waitpid(child, None).unwrap();
// 
//                 match exit_status {
//                     wait::WaitStatus::Exited(_pid, 0) => {} // normal exit
//                     wait::WaitStatus::Exited(_pid, err_code) => {
//                         panic!(
//                             "Child exited with an error {err_code}, run this test with \
//                                --nocapture to see the full output.",
//                         );
//                     }
//                     status => panic!("Unexpected child status: {status:?}"),
//                 }
//             }
//             Err(x) => panic!("{x}"),
//         }
//     }
// 
//     #[test]
//     fn write_protected_memory() {
//         let var: usize = 101;
//         let var2: u8 = 102;
// 
//         // Allocate an empty page and make it read-only
//         let layout = Layout::from_size_align(2 * *PAGE_SIZE, *PAGE_SIZE).unwrap();
//         let (write_protected_ptr, write_protected_ptr2) = unsafe {
//             let ptr = alloc_zeroed(layout);
//             mprotect(ptr as *mut std::ffi::c_void, *PAGE_SIZE, ProtFlags::PROT_READ)
//                 .expect("Failed to mprotect");
// 
//             (ptr as *const usize, ptr.add(std::mem::size_of::<usize>()))
//         };
// 
//         match unsafe { fork() } {
//             Ok(ForkResult::Child) => {
//                 ptrace::traceme().unwrap();
// 
//                 unsafe {
//                     assert_eq!(ptr::read_volatile(write_protected_ptr), 0);
//                     assert_eq!(ptr::read_volatile(write_protected_ptr2), 0);
//                 }
// 
//                 // Wait for the parent process to signal to continue.
//                 signal::raise(Signal::SIGSTOP).unwrap();
// 
//                 // Catch the panic so that we can report back to the original process.
//                 let test_res = std::panic::catch_unwind(|| unsafe {
//                     assert_eq!(ptr::read_volatile(write_protected_ptr), var);
//                     assert_eq!(ptr::read_volatile(write_protected_ptr2), var2);
//                 });
// 
//                 // Return an explicit status code.
//                 std::process::exit(if test_res.is_ok() { 0 } else { 100 });
//             }
//             Ok(ForkResult::Parent { child, .. }) => {
//                 // Wait for child.
//                 let wait_status = wait::waitpid(child, None).unwrap();
// 
//                 match wait_status {
//                     wait::WaitStatus::Stopped(_pid, _sig) => {}
//                     status => {
//                         signal::kill(child, Signal::SIGKILL).unwrap();
//                         panic!("Unexpected child status: {:?}", status);
//                     }
//                 }
// 
//                 let mut debugger = Debugger::<&str>::me();
//                 debugger.view(child).unwrap();
//                 let mut debugger = debugger.lock();
//                 let process = debugger.processes().next().expect("No processes");
// 
//                 // Write memory to the child's process.
//                 WriteMemory::new(process)
//                     .write(&var, write_protected_ptr as usize)
//                     .write(&var2, write_protected_ptr2 as usize)
//                     .apply()
//                     .unwrap();
// 
//                 ptrace::detach(child, Some(Signal::SIGCONT)).unwrap();
// 
//                 // 'Unprotect' memory so that it can be deallocated.
//                 unsafe {
//                     mprotect(
//                         write_protected_ptr as *mut _,
//                         *PAGE_SIZE,
//                         ProtFlags::PROT_WRITE | ProtFlags::PROT_READ,
//                     )
//                     .expect("Failed to mprotect");
//                     dealloc(write_protected_ptr as *mut _, layout);
//                 }
// 
//                 // Check if the child assertions are successful.
//                 let exit_status = wait::waitpid(child, None).unwrap();
// 
//                 match exit_status {
//                     wait::WaitStatus::Exited(_pid, 0) => {} // normal exit
//                     wait::WaitStatus::Exited(_pid, err_code) => {
//                         panic!(
//                             "Child exited with an error {err_code}, run this test with \
//                                --nocapture to see the full output.",
//                         );
//                     }
//                     status => panic!("Unexpected child status: {status:?}"),
//                 }
//             }
//             Err(x) => panic!("{x}"),
//         };
//     }
// 
//     /// Tests transformation of `WriteOp` into groups of words suitable for use in `ptrace::write`.
//     #[test]
//     fn ptrace_write_groups() {
//         let arr = [42u64, 64u64];
// 
//         let write_op = WriteOp {
//             remote_base: 0x100,
//             local_ptr: &arr[0] as *const _ as *mut u8,
//             local_len: std::mem::size_of_val(&arr),
//         };
// 
//         assert_eq!(
//             &write_op.into_word_sized_ops().collect::<Vec<_>>()[..],
//             &[
//                 WriteOp {
//                     remote_base: 0x100,
//                     local_ptr: &arr[0] as *const _ as *mut u8,
//                     local_len: std::mem::size_of::<u64>(),
//                 },
//                 WriteOp {
//                     remote_base: 0x100 + std::mem::size_of::<u64>(),
//                     local_ptr: &arr[1] as *const _ as *mut u8,
//                     local_len: std::mem::size_of::<u64>(),
//                 }
//             ][..]
//         );
//     }
// 
//     /// Tests transformation of `WriteOp` into groups suitable for use in `ptrace::write`.
//     /// Check that the uneven-sized write operations break down into correct groups.
//     #[test]
//     fn ptrace_write_groups_packed() {
//         #[repr(packed(2))]
//         struct PackedStruct {
//             _v1: u64,
//             _v2: u16,
//         }
//         let val = PackedStruct { _v1: 42, _v2: 256 };
// 
//         // Calculate offsets for fields `v1` and `v2`.
//         // FIXME: Replace with offset_of!(..) whenever it stabilizes.
//         let local_ptr = &val as *const PackedStruct as *mut u8;
// 
//         let write_op = WriteOp {
//             remote_base: 0x100,
//             local_ptr,
//             local_len: std::mem::size_of_val(&val),
//         };
// 
//         assert_eq!(
//             &write_op.into_word_sized_ops().collect::<Vec<_>>()[..],
//             &[
//                 WriteOp {
//                     remote_base: 0x100,
//                     local_ptr,
//                     local_len: std::mem::size_of::<u64>(),
//                 },
//                 WriteOp {
//                     remote_base: 0x100 + std::mem::size_of::<u64>(),
//                     local_ptr: unsafe { local_ptr.offset(std::mem::size_of::<u64>() as isize) },
//                     local_len: std::mem::size_of::<u16>(),
//                 }
//             ][..]
//         );
//     }
// }
