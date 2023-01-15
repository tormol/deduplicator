/* Copyright 2023 Torbjørn Birch Moltu
 *
 * This file is part of Deduplicator.
 * Deduplicator is free software: you can redistribute it and/or modify it under the
 * terms of the GNU General Public License as published by the Free Software Foundation,
 * either version 3 of the License, or (at your option) any later version.
 *
 * Deduplicator is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY;
 * without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.
 * See the GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License along with Deduplicator.
 * If not, see <https://www.gnu.org/licenses/>.
 */

extern crate clap;
#[cfg(target_os="linux")]
extern crate ioprio;
extern crate sha2;
extern crate thread_priority;

mod available_buffers;
use available_buffers::*;

use std::{fs, io::Read, process::exit, thread, time::Duration};
use std::num::{NonZeroU16, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, mpsc};

use clap::Parser;

use sha2::{Sha256, Digest};

use thread_priority::{ThreadBuilder, ThreadPriority};
#[cfg(unix)]
use thread_priority::unix::{NormalThreadSchedulePolicy, ThreadSchedulePolicy};

#[derive(Parser, Debug)]
#[command(arg_required_else_help=true, author, version, about, long_about=None)]
struct Args {
    #[arg(short, long, value_name="NUMBER_OF_IO_THREADS", default_value_t=NonZeroU16::new(2).unwrap())]
    io_threads: NonZeroU16,
    #[arg(short='t', long, value_name="NUBMER_OF_HASHER_THREADS", default_value_t=NonZeroU16::new(4).unwrap())]
    hasher_threads: NonZeroU16,
    #[arg(short='b', long, value_name="MAX_BUFFER_SIZE_IN_KB", default_value_t=NonZeroUsize::new(1024).unwrap())]
    max_buffer_size: NonZeroUsize,
    #[arg(short, long, value_name="MAX_MEMORY_USAGE_OF_BUFFERS_IN_MB")]
    max_buffers_memory: Option<NonZeroUsize>,
    #[arg(required = true)]
    roots: Vec<PathBuf>,
}

/// A vector that is always fully initialized.
#[derive(Default, Debug)]
struct UsedBuffer {
    buffer: Box<[u8]>,
    length: usize,
}

#[derive(Clone, Copy, Debug)]
enum ReadType {File, Directory}

#[derive(Debug)]
struct ReadQueue {
    queue: Vec<(PathBuf, ReadType)>,
    stop_now: bool,
    working: u32,
}
impl Default for ReadQueue {
    fn default() -> Self {
        ReadQueue { queue: Vec::new(), stop_now: false, working: 0, }
    }
}

#[derive(Debug)]
struct HashQueue {
    queue: Vec<(PathBuf, mpsc::Receiver<UsedBuffer>)>,
    stop_now: bool,
    stop_when_empty: bool,
}
impl Default for HashQueue {
    fn default() -> Self {
        HashQueue { queue: Vec::new(), stop_now: false, stop_when_empty: false, }
    }
}

#[derive(Debug)]
struct Pools {
    to_read: Mutex<ReadQueue>,
    reader_waker: Condvar,
    to_hash: Mutex<HashQueue>,
    hasher_waker: Condvar,
    buffers: AvailableBuffers,
}

fn read_dir(dir_path: PathBuf,  pool: &Pools) {
    let entries = fs::read_dir(&dir_path).unwrap_or_else(|e| {
        eprintln!("Cannot open {}: {}", dir_path.display(), e);
        exit(1);
    });
    for entry in entries {
        let entry = entry.unwrap_or_else(|e| {
            eprintln!("Error getting entry from {}: {}", dir_path.display(), e);
            exit(1);
        });
        let mut entry_path = dir_path.clone();
        entry_path.push(entry.path());
        let file_type = entry.file_type().unwrap_or_else(|e| {
            eprintln!("Error getting type of {}: {}", entry_path.display(), e);
            exit(1);
        });
        let file_type = if file_type.is_file() {
            ReadType::File
        } else if file_type.is_dir() {
            ReadType::Directory
        } else {
            let file_type = if file_type.is_symlink() {"symlink"} else {"special file"};
            println!("{} is a {}, skipping.", entry_path.display(), file_type);
            continue;
        };
        let mut lock = pool.to_read.lock().unwrap();
        lock.queue.push((entry_path, file_type));
        drop(lock);
        pool.reader_waker.notify_one();
    }
}

fn read_file(file_path: &Path,  thread_name: &str,  pool: &Pools) {
    let mut file = match fs::File::open(file_path) {
        Ok(file) => file,
        Err(e) => {
            eprintln!("Cannot open  {}: {}", file_path.display(), e);
            return;
        }
    };

    let mut buf = pool.buffers.get_buffer(pool.buffers.max_single_buffer_size(), thread_name);
    let (tx, rx) = mpsc::channel();
    let mut insert_rx = Some(rx);
    let mut pos = 0;
    let mut incomplete = true;

    while incomplete {
        match file.read(&mut buf) {
            Err(e) => {
                println!("{} reading failed after {} bytes: {}", file_path.display(), pos, e);
                let empty = UsedBuffer {
                    buffer: Box::default(),
                    length: 0,
                };
                tx.send(empty).unwrap();
                incomplete = false;
            }
            Ok(0) => {
                incomplete = false;
            }
            Ok(n) => {
                let read = UsedBuffer {
                    buffer: buf,
                    length: n,
                };
                buf = Box::default();
                pos += n;
                tx.send(read).unwrap();
            }
        }
        if let Some(rx) = insert_rx.take() {
            let mut lock = pool.to_hash.lock().unwrap();
            lock.queue.push((file_path.to_owned(), rx));
            drop(lock);
            pool.hasher_waker.notify_one();
        }
    }
    pool.buffers.return_buffer(buf);
}

fn read_files(pool: Arc<Pools>, thread_name: String) {
    let mut lock = pool.to_read.lock().unwrap();

    loop {
        if lock.stop_now {
            eprintln!("{} quit due to stop signal", thread_name);
            break;
        } else if let Some((path, ty)) = lock.queue.pop() {
            lock.working += 1;
            drop(lock);

            println!("{} reading {}", thread_name, path.display());
            match ty {
                ReadType::File => read_file(&path, &thread_name, &pool),
                ReadType::Directory => read_dir(path, &pool),
            }

            lock = pool.to_read.lock().unwrap();
            lock.working -= 1;
        } else if lock.working == 0 {
            eprintln!("{} quit due to no more work", thread_name);
            break;
        } else {
            lock = pool.reader_waker.wait(lock).unwrap();
        }
    }
}

fn hash_file(
        file_path: PathBuf,  chunks: mpsc::Receiver<UsedBuffer>,
        hasher: &mut sha2::Sha256,  thread_name: &str,
        buffers: &AvailableBuffers) {
    let buf = match chunks.try_recv() {
        Ok(buf) => buf,
        Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => {
            println!("{} is empty", file_path.display());
            return;
        },
    };

    println!("{} hashing {}", thread_name, file_path.display());
    hasher.update(&buf.buffer[..buf.length]);
    let mut size = buf.length;
    buffers.return_buffer(buf.buffer);

    for buf in chunks.into_iter() {
        if buf.length == 0 {
            // IO error
            hasher.reset();
            return;
        }
        hasher.update(&buf.buffer[..buf.length]);
        size += buf.length;
        buffers.return_buffer(buf.buffer);
    }

    let hash_result = hasher.finalize_reset();
    println!("{} {} bytes {:#x}", file_path.display(), size, hash_result);
}

fn hash_files(pool: Arc<Pools>,  thread_name: String) {
    let mut hasher = Sha256::new();
    let mut lock = pool.to_hash.lock().unwrap();

    loop {
        if lock.stop_now {
            eprintln!("{} quit due to stop signal", thread_name);
            break;
        } else if let Some((path, rx)) = lock.queue.pop() {
            drop(lock);
            hash_file(path, rx, &mut hasher, &thread_name, &pool.buffers);
            lock = pool.to_hash.lock().unwrap();
        } else if lock.stop_when_empty {
            eprintln!("{} quit due to no more work", thread_name);
            break;
        } else {
            lock = pool.hasher_waker.wait(lock).unwrap();
        }
    }
}

fn main() {
    let args = Args::parse();
    let pool = Arc::new(Pools {
        to_read: Mutex::new(ReadQueue::default()),
        reader_waker: Condvar::new(),
        to_hash: Mutex::new(HashQueue::default()),
        hasher_waker: Condvar::new(),
        buffers: available_buffers::AvailableBuffers::new(
            match args.max_buffers_memory {
                Some(size) => usize::from(size).saturating_mul(1024*1024),
                None => isize::MAX as usize,
            },
            usize::from(args.max_buffer_size).saturating_mul(1024),
        ),
    });

    // check root directories and add them to queue
    let mut to_read = pool.to_read.lock().unwrap();
    for dir_path in args.roots {
        let dir_path = fs::canonicalize(&dir_path).unwrap_or_else(|e| {
            eprintln!("Cannot canoniicalize {}: {}", dir_path.display(), e);
            exit(1);
        });
        to_read.queue.push((dir_path, ReadType::Directory));
    }
    drop(to_read);

    // Keep my desktop responsive
    #[cfg(target_os="linux")]
    {
        let this = ioprio::Target::Process(ioprio::Pid::this());
        let priority = ioprio::Class::BestEffort(ioprio::BePriorityLevel::lowest());
        if let Err(e) = ioprio::set_priority(this, ioprio::Priority::new(priority)) {
            eprintln!("Failed to set IO priority to {:?}: {}", priority, e);
        }
    }

    // start hasher threads
    let mut hasher_threads = Vec::with_capacity(u16::from(args.hasher_threads).into());
    for n in (1..=args.hasher_threads.into()).into_iter() {
        let thread_name = format!("hasher_{}", n);
        let pool = pool.clone();
        let builder = ThreadBuilder::default()
            .name(thread_name.clone())
            .priority(ThreadPriority::Min);
        #[cfg(unix)]
        let builder = builder.policy(
                ThreadSchedulePolicy::Normal(NormalThreadSchedulePolicy::Batch)
        );
        let thread = builder.spawn(move |priority_result| {
            if let Err(e) = priority_result {
                eprintln!("Failed lowering thread priority: {:?}", e);
            }
            hash_files(pool, thread_name)
        }).unwrap();
        hasher_threads.push(thread);
    }

    // start IO threads
    let mut io_threads = Vec::with_capacity(u16::from(args.io_threads).into());
    for n in (1..=args.io_threads.into()).into_iter() {
        let thread_name = format!("io_{}", n);
        let pool = pool.clone();
        let builder = thread::Builder::new().name(thread_name.clone());
        let thread = builder.spawn(move || read_files(pool, thread_name) ).unwrap();
        io_threads.push(thread);
    }

    // wait for IO threads to finish
    loop {
        eprintln!("buffer memory allocated: {} MiB",
                pool.buffers.current_buffers_size()/(1024*1024),
        );

        let lock = pool.to_read.lock().unwrap();
        if (lock.queue.is_empty() && lock.working == 0) || lock.stop_now {
            break;
        }
        drop(lock);
        thread::sleep(Duration::from_millis(500));
    }

    // tell hashers they can stop now
    let mut lock = pool.to_hash.lock().unwrap();
    lock.stop_when_empty = true;
    drop(lock);

    pool.reader_waker.notify_all();
    for thread in io_threads {
        eprintln!("joining reader");
        thread.join().unwrap();
    }

    pool.hasher_waker.notify_all();
    for thread in hasher_threads {
        eprintln!("joining hasher");
        thread.join().unwrap();
    }
}