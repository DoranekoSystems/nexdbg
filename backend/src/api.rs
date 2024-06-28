use crate::allocator;
use crate::api::lz4::EncoderBuilder;
use crate::util;
use crate::util::binary_search;
use aho_corasick::AhoCorasick;
use byteorder::{ByteOrder, LittleEndian};
use hex;
use lazy_static::lazy_static;
use libc::int8_t;
use libc::{self, c_char, c_int, c_long, c_void, off_t, O_RDONLY};
use lz4;
use lz4::{block::compress, BlockMode};
use memchr::{memmem, Memchr};
use rayon::prelude::*;
use regex::bytes::Regex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::env;
use std::ffi::CStr;
use std::ffi::CString;
use std::fs::File;
use std::io::Error;
use std::io::{BufRead, BufReader};
use std::process;
use std::str;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::RwLock;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use warp::hyper::Body;
use warp::redirect::found;
use warp::{http::Response, http::StatusCode, Filter, Rejection, Reply};

#[cfg_attr(target_os = "android", link(name = "c++_static", kind = "static"))]
#[cfg_attr(target_os = "android", link(name = "c++abi", kind = "static"))]
#[link(name = "native", kind = "static")]
extern "C" {
    fn get_pid_native() -> i32;
    fn enumprocess_native(count: *mut usize) -> *mut ProcessInfo;
    fn enummodule_native(pid: i32, count: *mut usize) -> *mut ModuleInfo;
    fn enumerate_regions_to_buffer(pid: i32, buffer: *mut u8, buffer_size: usize);
    fn read_memory_native(
        pid: libc::c_int,
        address: libc::uintptr_t,
        size: libc::size_t,
        buffer: *mut u8,
    ) -> libc::ssize_t;
    fn write_memory_native(
        pid: i32,
        address: libc::uintptr_t,
        size: libc::size_t,
        buffer: *const u8,
    ) -> libc::ssize_t;
    fn suspend_process(pid: i32) -> bool;
    fn resume_process(pid: i32) -> bool;
    fn native_init() -> libc::c_void;
}

#[repr(C)]
struct ProcessInfo {
    pid: i32,
    processname: *mut c_char,
}

#[repr(C)]
struct ModuleInfo {
    base: usize,
    size: i32,
    is_64bit: bool,
    modulename: *mut c_char,
}

lazy_static! {
    static ref GLOBAL_POSITIONS: RwLock<HashMap<String, Vec<(usize, String)>>> =
        RwLock::new(HashMap::new());
    static ref GLOBAL_MEMORY: RwLock<HashMap<String, Vec<(usize, Vec<u8>, usize, Vec<u8>, usize, bool)>>> =
        RwLock::new(HashMap::new());
    static ref GLOBAL_SCAN_OPTION: RwLock<HashMap<String, MemoryScanRequest>> =
        RwLock::new(HashMap::new());
}

pub fn with_state(
    state: Arc<Mutex<Option<i32>>>,
) -> impl Filter<Extract = (Arc<Mutex<Option<i32>>>,), Error = std::convert::Infallible> + Clone {
    warp::any().map(move || state.clone())
}

const MAX_RESULTS: usize = 100_000;

#[derive(Serialize)]
struct ServerInfo {
    git_hash: String,
    target_os: String,
    arch: String,
    pid: u32,
    mode: String,
}

pub async fn server_info_handler() -> Result<impl warp::Reply, warp::Rejection> {
    let git_hash = env!("GIT_HASH");
    let target_os = env!("TARGET_OS");

    let arch = if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else if cfg!(target_arch = "arm") {
        "arm"
    } else if cfg!(target_arch = "x86") {
        "x86"
    } else {
        "unknown"
    };

    let pid = process::id();

    let server_info = ServerInfo {
        git_hash: git_hash.to_string(),
        target_os: target_os.to_string(),
        arch: arch.to_string(),
        pid: pid,
        mode: std::env::var("MEMORY_SERVER_RUNNING_MODE").unwrap_or_else(|_| "unknown".to_string()),
    };

    Ok(warp::reply::json(&server_info))
}

#[derive(Deserialize)]
pub struct OpenProcess {
    pid: i32,
}

pub async fn open_process_handler(
    pid_state: Arc<Mutex<Option<i32>>>,
    open_process: OpenProcess,
) -> Result<impl warp::Reply, warp::Rejection> {
    let mut pid = pid_state.lock().unwrap();
    *pid = Some(open_process.pid);
    Ok(warp::reply::with_status("OK", warp::http::StatusCode::OK))
}

#[derive(Deserialize)]
pub struct ReadMemoryRequest {
    address: usize,
    size: usize,
}

pub async fn read_memory_handler(
    pid_state: Arc<Mutex<Option<i32>>>,
    read_memory: ReadMemoryRequest,
) -> Result<impl warp::Reply, warp::Rejection> {
    let pid = pid_state.lock().unwrap();

    if let Some(pid) = *pid {
        let mut buffer: Vec<u8> = vec![0; read_memory.size];
        let nread = read_process_memory(
            pid,
            read_memory.address as *mut libc::c_void,
            read_memory.size,
            &mut buffer,
        );
        match nread {
            Ok(_) => {
                let response = Response::builder()
                    .header("Content-Type", "application/octet-stream")
                    .body(hyper::Body::from(buffer))
                    .unwrap();
                return Ok(response);
            }
            Err(_) => {
                let empty_buffer = Vec::new();
                let response = Response::builder()
                    .header("Content-Type", "application/octet-stream")
                    .body(hyper::Body::from(empty_buffer))
                    .unwrap();
                return Ok(response);
            }
        };
    } else {
        let response = Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(hyper::Body::from("Pid not set"))
            .unwrap();
        Ok(response)
    }
}

pub async fn read_memory_multiple_handler(
    pid_state: Arc<Mutex<Option<i32>>>,
    read_memory_requests: Vec<ReadMemoryRequest>,
) -> Result<impl warp::Reply, warp::Rejection> {
    let pid = pid_state.lock().unwrap();
    if let Some(pid) = *pid {
        let compressed_buffers: Vec<Vec<u8>> = read_memory_requests
            .par_iter()
            .map(|request| {
                let mut buffer: Vec<u8> = vec![0; request.size];
                let nread = read_process_memory(
                    pid,
                    request.address as *mut libc::c_void,
                    request.size,
                    &mut buffer,
                );
                match nread {
                    Ok(_) => {
                        let compressed_buffer = lz4::block::compress(&buffer, None, true).unwrap();
                        let mut result_buffer = Vec::with_capacity(8 + compressed_buffer.len());
                        let compresed_buffer_size: u32 = compressed_buffer.len() as u32;
                        result_buffer.extend_from_slice(&1u32.to_le_bytes());
                        result_buffer.extend_from_slice(&compresed_buffer_size.to_le_bytes());
                        result_buffer.extend_from_slice(&compressed_buffer);
                        result_buffer
                    }
                    Err(_) => {
                        let mut result_buffer = Vec::with_capacity(4);
                        result_buffer.extend_from_slice(&0u32.to_le_bytes());
                        result_buffer
                    }
                }
            })
            .collect();

        let mut concatenated_buffer = Vec::new();
        for buffer in compressed_buffers {
            concatenated_buffer.extend(buffer);
        }

        let response = Response::builder()
            .header("Content-Type", "application/octet-stream")
            .body(hyper::Body::from(concatenated_buffer))
            .unwrap();
        Ok(response)
    } else {
        let response = Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(hyper::Body::from("Pid not set"))
            .unwrap();
        Ok(response)
    }
}

#[derive(Deserialize)]
pub struct WriteMemoryRequest {
    address: usize,
    buffer: Vec<u8>,
}

pub async fn write_memory_handler(
    pid_state: Arc<Mutex<Option<i32>>>,
    write_memory: WriteMemoryRequest,
) -> Result<impl warp::Reply, warp::Rejection> {
    let pid = pid_state.lock().unwrap();

    if let Some(pid) = *pid {
        let nwrite = write_process_memory(
            pid,
            write_memory.address as *mut libc::c_void,
            write_memory.buffer.len(),
            &write_memory.buffer,
        );
        match nwrite {
            Ok(_) => {
                let response = Response::builder()
                    .header("Content-Type", "text/plain")
                    .body(hyper::Body::from("Memory successfully written"))
                    .unwrap();
                return Ok(response);
            }
            Err(_) => {
                let response = Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .body(hyper::Body::from("WriteProcessMemory error"))
                    .unwrap();
                return Ok(response);
            }
        };
    } else {
        let response = Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(hyper::Body::from("Pid not set"))
            .unwrap();
        Ok(response)
    }
}

#[derive(Deserialize, Clone)]
pub struct MemoryScanRequest {
    pattern: String,
    address_ranges: Vec<(usize, usize)>,
    find_type: String,
    data_type: String,
    scan_id: String,
    align: usize,
    return_as_json: bool,
    do_suspend: bool,
}

pub async fn memory_scan_handler(
    pid_state: Arc<Mutex<Option<i32>>>,
    scan_request: MemoryScanRequest,
) -> Result<impl warp::Reply, warp::Rejection> {
    let pid = pid_state.lock().unwrap();

    let mut is_suspend_success: bool = false;
    let do_suspend = scan_request.do_suspend;
    if let Some(pid) = *pid {
        if do_suspend {
            unsafe {
                is_suspend_success = suspend_process(pid);
            }
        }
        // Clear global_positions for the given scan_id
        {
            let mut global_positions = GLOBAL_POSITIONS.write().unwrap();
            if let Some(positions) = global_positions.get_mut(&scan_request.scan_id) {
                positions.clear();
            }
            let mut global_memory = GLOBAL_MEMORY.write().unwrap();
            if let Some(memory) = global_memory.get_mut(&scan_request.scan_id) {
                memory.clear();
            } else {
            }
            let mut global_scan_option = GLOBAL_SCAN_OPTION.write().unwrap();
            global_scan_option.insert(scan_request.scan_id.clone(), scan_request.clone());
        }
        let is_number = match scan_request.data_type.as_str() {
            "int16" | "uint16" | "int32" | "uint32" | "float" | "int64" | "uint64" | "double" => {
                true
            }
            _ => false,
        };
        let found_count = Arc::new(AtomicUsize::new(0));
        let scan_align = scan_request.align;
        let thread_results: Vec<Vec<(usize, String)>> = scan_request
            .address_ranges
            .par_iter()
            .flat_map(|(start_address, end_address)| {
                let found_count = Arc::clone(&found_count);
                let size = end_address - start_address;
                let chunk_size = 1024 * 1024 * 16; // 16MB
                let num_chunks = (size + chunk_size - 1) / chunk_size;

                (0..num_chunks)
                    .map(|i| {
                        let chunk_start = start_address + i * chunk_size;
                        let chunk_end = std::cmp::min(chunk_start + chunk_size, *end_address);
                        let chunk_size_actual = chunk_end - chunk_start;
                        let mut buffer: Vec<u8> = vec![0; chunk_size_actual];

                        let mut local_positions = vec![];
                        let mut local_values = vec![];

                        let nread = match read_process_memory(
                            pid,
                            chunk_start as *mut libc::c_void,
                            chunk_size_actual,
                            &mut buffer,
                        ) {
                            Ok(nread) => nread,
                            Err(_) => -1,
                        };

                        if nread != -1 {
                            if scan_request.find_type == "exact" {
                                if scan_request.data_type == "regex" {
                                    let regex_pattern = &scan_request.pattern;
                                    let re = match Regex::new(regex_pattern) {
                                        Ok(re) => re,
                                        Err(_) => return vec![],
                                    };

                                    for cap in re.captures_iter(&buffer) {
                                        let start = cap.get(0).unwrap().start();
                                        if (chunk_start + start) % scan_align == 0 {
                                            let end = cap.get(0).unwrap().end();
                                            let value = hex::encode(&buffer[start..end]);
                                            local_positions.push(chunk_start + start);
                                            local_values.push(value);
                                            found_count.fetch_add(1, Ordering::SeqCst);
                                        }
                                    }
                                } else {
                                    let search_bytes = match hex::decode(&scan_request.pattern) {
                                        Ok(bytes) => bytes,
                                        Err(_) => return vec![],
                                    };

                                    let mut buffer_offset = 0;
                                    for pos in memmem::find_iter(&buffer, &search_bytes) {
                                        let start = chunk_start + buffer_offset + pos;
                                        if start % scan_align == 0 {
                                            let value = scan_request.pattern.clone();
                                            if is_number {
                                                local_positions.push(start);
                                                local_values.push(value);
                                            } else {
                                                local_positions.push(start);
                                                local_values.push(value);
                                            }
                                            found_count.fetch_add(1, Ordering::SeqCst);
                                        }
                                        buffer_offset += pos + 1;
                                    }
                                }
                            } else if scan_request.find_type == "unknown" {
                                let alignment = match scan_request.data_type.as_str() {
                                    "int16" | "uint16" => 2,
                                    "int32" | "uint32" | "float" => 4,
                                    "int64" | "uint64" | "double" => 8,
                                    _ => 1,
                                };
                                let mut global_memory = GLOBAL_MEMORY.write().unwrap();
                                let compressed_buffer =
                                    lz4::block::compress(&buffer, None, false).unwrap();

                                if let Some(memory) = global_memory.get_mut(&scan_request.scan_id) {
                                    memory.push((
                                        chunk_start,
                                        compressed_buffer,
                                        buffer.len(),
                                        vec![],
                                        0,
                                        true,
                                    ));
                                } else {
                                    global_memory.insert(
                                        scan_request.scan_id.clone(),
                                        vec![(
                                            chunk_start,
                                            compressed_buffer,
                                            buffer.len(),
                                            vec![],
                                            0,
                                            true,
                                        )],
                                    );
                                }
                                found_count.fetch_add(buffer.len() / alignment, Ordering::SeqCst);
                            }
                            // Check if local_positions exceed MAX_RESULTS and insert into global_positions
                            if local_positions.len() > MAX_RESULTS {
                                let mut global_positions = GLOBAL_POSITIONS.write().unwrap();
                                let combined: Vec<(usize, String)> = local_positions
                                    .into_iter()
                                    .zip(local_values.into_iter())
                                    .collect();
                                if let Some(positions) =
                                    global_positions.get_mut(&scan_request.scan_id)
                                {
                                    positions.extend(combined);
                                } else {
                                    global_positions.insert(scan_request.scan_id.clone(), combined);
                                }
                                local_positions = vec![];
                                local_values = vec![];
                            }
                        }

                        let combined: Vec<(usize, String)> = local_positions
                            .into_iter()
                            .zip(local_values.into_iter())
                            .collect();
                        combined
                    })
                    .collect::<Vec<_>>()
            })
            .collect();

        if do_suspend && is_suspend_success {
            unsafe {
                resume_process(pid);
            }
        }
        // println!("{}", found_count.load(Ordering::SeqCst));

        let flattened_results: Vec<(usize, String)> =
            thread_results.into_iter().flatten().collect();
        {
            let mut global_positions = GLOBAL_POSITIONS.write().unwrap();
            if let Some(positions) = global_positions.get_mut(&scan_request.scan_id) {
                positions.extend(flattened_results);
            } else {
                global_positions.insert(scan_request.scan_id.clone(), flattened_results);
            }
        }

        if scan_request.return_as_json {
            let global_positions = GLOBAL_POSITIONS.read().unwrap();
            if let Some(positions) = global_positions.get(&scan_request.scan_id) {
                let limited_positions = &positions[..std::cmp::min(MAX_RESULTS, positions.len())];
                let count = found_count.load(Ordering::SeqCst);
                let mut is_rounded: bool;
                if scan_request.find_type == "unknown" {
                    if count > 1_000_000 {
                        is_rounded = true;
                    } else {
                        is_rounded = limited_positions.len() != positions.len();
                    }
                } else {
                    is_rounded = limited_positions.len() != positions.len();
                }
                let matched_addresses: Vec<serde_json::Value> = limited_positions
                    .clone()
                    .into_iter()
                    .map(|(address, value)| {
                        json!({
                            "address": address,
                            "value": value
                        })
                    })
                    .collect();
                let result = json!({
                    "matched_addresses": matched_addresses,
                    "found":count,
                    "is_rounded":is_rounded
                });
                let result_string = result.to_string();
                let response = Response::builder()
                    .header("Content-Type", "application/json")
                    .body(hyper::Body::from(result_string))
                    .unwrap();
                Ok(response)
            } else {
                let response = Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .body(hyper::Body::from("Unknown error"))
                    .unwrap();
                Ok(response)
            }
        } else {
            let global_positions = GLOBAL_POSITIONS.read().unwrap();
            if let Some(positions) = global_positions.get(&scan_request.scan_id) {
                let count = found_count.load(Ordering::SeqCst);
                let result_string = json!({ "found": count }).to_string();
                let response = Response::builder()
                    .header("Content-Type", "application/json")
                    .body(hyper::Body::from(result_string))
                    .unwrap();
                Ok(response)
            } else {
                let response = Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .body(hyper::Body::from("Unknown error"))
                    .unwrap();
                Ok(response)
            }
        }
    } else {
        let response = Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(hyper::Body::from("Pid not set"))
            .unwrap();
        Ok(response)
    }
}

macro_rules! compare_values {
    ($val:expr, $old_val:expr, $filter_method:expr) => {
        match $filter_method {
            "changed" => $val != $old_val,
            "unchanged" => $val == $old_val,
            "increased" => $val > $old_val,
            "decreased" => $val < $old_val,
            _ => false,
        }
    };
}

#[derive(Deserialize)]
pub struct MemoryFilterRequest {
    pattern: String,
    data_type: String,
    scan_id: String,
    filter_method: String,
    return_as_json: bool,
    do_suspend: bool,
}

pub async fn memory_filter_handler(
    pid_state: Arc<Mutex<Option<i32>>>,
    filter_request: MemoryFilterRequest,
) -> Result<impl warp::Reply, warp::Rejection> {
    let pid = pid_state.lock().unwrap();

    let mut is_suspend_success: bool = false;
    let do_suspend = filter_request.do_suspend;
    if let Some(pid) = *pid {
        let mut new_positions = Vec::new();
        let mut global_positions = GLOBAL_POSITIONS.write().unwrap();
        let mut global_memory = GLOBAL_MEMORY.write().unwrap();
        let mut global_scan_option = GLOBAL_SCAN_OPTION.write().unwrap();
        let scan_option: MemoryScanRequest = global_scan_option
            .get(&filter_request.scan_id)
            .unwrap()
            .clone();
        let found_count = Arc::new(AtomicUsize::new(0));
        let size = match filter_request.data_type.as_str() {
            "int16" | "uint16" => 2,
            "int32" | "uint32" | "float" => 4,
            "int64" | "uint64" | "double" => 8,
            _ => 1,
        };
        // unknown search
        if let Some(memory) = global_memory.get_mut(&filter_request.scan_id) {
            if do_suspend {
                unsafe {
                    is_suspend_success = suspend_process(pid);
                }
            }
            let scan_align = scan_option.align;
            memory.par_iter_mut().for_each(|entry| {
                let (
                    address,
                    compressed_data,
                    uncompressed_data_size,
                    compressed_offsets,
                    uncompressed_offsets_size,
                    is_first,
                ) = entry;
                let mut local_positions = vec![];
                let decompressed_data =
                    lz4::block::decompress(compressed_data, Some(*uncompressed_data_size as i32))
                        .unwrap();
                let mut decompressed_offsets: Vec<i32>;
                let mut buffer: Vec<u8> = vec![0; (decompressed_data.len()) as usize];
                let _nread = match read_process_memory(
                    pid,
                    *address as *mut libc::c_void,
                    decompressed_data.len(),
                    &mut buffer,
                ) {
                    Ok(nread) => nread,
                    Err(err) => -1,
                };

                if _nread == -1 {
                    return;
                }

                if *is_first {
                    for offset in (0..decompressed_data.len()).step_by(1) {
                        if (*address + offset) % scan_align != 0 {
                            continue;
                        }
                        if offset + size > decompressed_data.len() {
                            break;
                        }
                        let old_val = &decompressed_data[offset..offset + size];
                        let new_val = &buffer[offset..offset + size];

                        let pass_filter = match filter_request.data_type.as_str() {
                            _ => compare_values!(
                                new_val,
                                old_val,
                                filter_request.filter_method.as_str()
                            ),
                        };
                        if pass_filter {
                            local_positions.push(offset);
                            found_count.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                    let offsets_as_bytes: Vec<u8> = local_positions
                        .iter()
                        .flat_map(|&x| x.to_le_bytes().to_vec())
                        .collect();
                    *compressed_offsets = compress(&offsets_as_bytes, None, false).unwrap();
                    *uncompressed_offsets_size =
                        local_positions.len() * std::mem::size_of::<usize>();
                    *is_first = false;
                    let compressed_buffer = lz4::block::compress(&buffer, None, false).unwrap();
                    *compressed_data = compressed_buffer.clone();
                    *uncompressed_data_size = buffer.len();
                } else {
                    let decompressed_offsets_buffer = lz4::block::decompress(
                        compressed_offsets,
                        Some(*uncompressed_offsets_size as i32),
                    )
                    .unwrap();

                    let decompressed_offsets: Vec<usize> = decompressed_offsets_buffer
                        .chunks_exact(8)
                        .map(|chunk| usize::from_le_bytes(chunk.try_into().unwrap()))
                        .collect();
                    for offset in decompressed_offsets {
                        if (*address + offset) % scan_align != 0 {
                            continue;
                        }
                        if offset + size > decompressed_data.len() {
                            break;
                        }
                        let old_val = &decompressed_data[offset..offset + size];
                        let new_val = &buffer[offset..offset + size];

                        let pass_filter = match filter_request.data_type.as_str() {
                            _ => compare_values!(
                                new_val,
                                old_val,
                                filter_request.filter_method.as_str()
                            ),
                        };
                        if pass_filter {
                            local_positions.push(offset);
                            found_count.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                    let offsets_as_bytes: Vec<u8> = local_positions
                        .iter()
                        .flat_map(|&x| x.to_le_bytes().to_vec())
                        .collect();
                    *compressed_offsets = compress(&offsets_as_bytes, None, false).unwrap();
                    *uncompressed_offsets_size =
                        local_positions.len() * std::mem::size_of::<usize>();
                    let compressed_buffer = lz4::block::compress(&buffer, None, false).unwrap();
                    *compressed_data = compressed_buffer.clone();
                    *uncompressed_data_size = buffer.len();
                }
            });
            if found_count.load(Ordering::SeqCst) < 1_000_000 {
                let mut results: Vec<_> = memory
                    .par_iter()
                    .flat_map(
                        |(
                            address,
                            compressed_data,
                            uncompressed_data_size,
                            compressed_offsets,
                            uncompressed_offsets_size,
                            _,
                        )| {
                            let mut local_positions = vec![];
                            if *uncompressed_offsets_size == 0 {
                                return local_positions;
                            }

                            let decompressed_data = lz4::block::decompress(
                                compressed_data,
                                Some(*uncompressed_data_size as i32),
                            )
                            .unwrap();
                            let decompressed_offsets_buffer = lz4::block::decompress(
                                compressed_offsets,
                                Some(*uncompressed_offsets_size as i32),
                            )
                            .unwrap();
                            let decompressed_offsets: Vec<usize> = decompressed_offsets_buffer
                                .chunks_exact(8)
                                .map(|chunk| usize::from_le_bytes(chunk.try_into().unwrap()))
                                .collect();
                            for offset in decompressed_offsets {
                                let val = &decompressed_data[offset..offset + size];
                                local_positions.push((*address + offset, hex::encode(val)));
                            }
                            local_positions
                        },
                    )
                    .collect();

                results.sort_by_key(|k| k.0);
                new_positions = results;
                global_memory.remove(&filter_request.scan_id);
            }
        } else if let Some(positions) = global_positions.get(&filter_request.scan_id) {
            if do_suspend {
                unsafe {
                    is_suspend_success = suspend_process(pid);
                }
            }
            let results: Result<Vec<_>, _> = positions
                .par_iter()
                .map(|(address, value)| {
                    let mut buffer: Vec<u8> = vec![0; (value.len() / 2) as usize];
                    let _nread = match read_process_memory(
                        pid,
                        *address as *mut libc::c_void,
                        filter_request.pattern.len(),
                        &mut buffer,
                    ) {
                        Ok(nread) => nread,
                        Err(err) => -1,
                    };

                    if _nread == -1 {
                        return Ok(None);
                    }

                    if filter_request.data_type == "regex" {
                        let regex_pattern = &filter_request.pattern;
                        let re = match Regex::new(regex_pattern) {
                            Ok(re) => re,
                            Err(_) => return Ok(None),
                        };
                        if re.is_match(&buffer) {
                            found_count.fetch_add(1, Ordering::SeqCst);
                            return Ok(Some((*address, hex::encode(&buffer))));
                        }
                    } else {
                        if filter_request.filter_method == "exact" {
                            let result = hex::decode(&filter_request.pattern);
                            let bytes = match result {
                                Ok(bytes) => bytes,
                                Err(_) => {
                                    let response = Response::builder()
                                        .status(StatusCode::BAD_REQUEST)
                                        .body(hyper::Body::from("Invalid hex pattern"))
                                        .unwrap();
                                    return Err(response);
                                }
                            };
                            if buffer == bytes {
                                found_count.fetch_add(1, Ordering::SeqCst);
                                return Ok(Some((*address, hex::encode(&buffer))));
                            }
                        } else {
                            let result = hex::decode(&value);
                            let bytes = match result {
                                Ok(bytes) => bytes,
                                Err(_) => {
                                    let response = Response::builder()
                                        .status(StatusCode::BAD_REQUEST)
                                        .body(hyper::Body::from("Invalid hex pattern"))
                                        .unwrap();
                                    return Err(response);
                                }
                            };
                            let mut pass_filter = false;

                            pass_filter = match filter_request.data_type.as_str() {
                                "int8" => {
                                    let old_val = i8::from_le_bytes(bytes.try_into().unwrap());
                                    let val = i8::from_le_bytes(buffer.clone().try_into().unwrap());
                                    compare_values!(
                                        val,
                                        old_val,
                                        filter_request.filter_method.as_str()
                                    )
                                }
                                "uint8" => {
                                    let old_val = u8::from_le_bytes(bytes.try_into().unwrap());
                                    let val = u8::from_le_bytes(buffer.clone().try_into().unwrap());
                                    compare_values!(
                                        val,
                                        old_val,
                                        filter_request.filter_method.as_str()
                                    )
                                }
                                "int16" => {
                                    let old_val = i16::from_le_bytes(bytes.try_into().unwrap());
                                    let val =
                                        i16::from_le_bytes(buffer.clone().try_into().unwrap());
                                    compare_values!(
                                        val,
                                        old_val,
                                        filter_request.filter_method.as_str()
                                    )
                                }
                                "uint16" => {
                                    let old_val = u16::from_le_bytes(bytes.try_into().unwrap());
                                    let val =
                                        u16::from_le_bytes(buffer.clone().try_into().unwrap());
                                    compare_values!(
                                        val,
                                        old_val,
                                        filter_request.filter_method.as_str()
                                    )
                                }
                                "int32" => {
                                    let old_val = i32::from_le_bytes(bytes.try_into().unwrap());
                                    let val =
                                        i32::from_le_bytes(buffer.clone().try_into().unwrap());
                                    compare_values!(
                                        val,
                                        old_val,
                                        filter_request.filter_method.as_str()
                                    )
                                }
                                "uint32" => {
                                    let old_val = u32::from_le_bytes(bytes.try_into().unwrap());
                                    let val =
                                        u32::from_le_bytes(buffer.clone().try_into().unwrap());
                                    compare_values!(
                                        val,
                                        old_val,
                                        filter_request.filter_method.as_str()
                                    )
                                }
                                "int64" => {
                                    let old_val = i64::from_le_bytes(bytes.try_into().unwrap());
                                    let val =
                                        i64::from_le_bytes(buffer.clone().try_into().unwrap());
                                    compare_values!(
                                        val,
                                        old_val,
                                        filter_request.filter_method.as_str()
                                    )
                                }
                                "uint64" => {
                                    let old_val = u64::from_le_bytes(bytes.try_into().unwrap());
                                    let val =
                                        u64::from_le_bytes(buffer.clone().try_into().unwrap());
                                    compare_values!(
                                        val,
                                        old_val,
                                        filter_request.filter_method.as_str()
                                    )
                                }
                                "float" => {
                                    let old_val = LittleEndian::read_f32(&bytes);
                                    let val = LittleEndian::read_f32(&buffer.clone());
                                    compare_values!(
                                        val,
                                        old_val,
                                        filter_request.filter_method.as_str()
                                    )
                                }
                                "double" => {
                                    let old_val = LittleEndian::read_f64(&bytes);
                                    let val = LittleEndian::read_f64(&buffer.clone());
                                    compare_values!(
                                        val,
                                        old_val,
                                        filter_request.filter_method.as_str()
                                    )
                                }
                                "utf-8" => {
                                    let old_val = str::from_utf8(&bytes).unwrap_or("");
                                    let val = str::from_utf8(&buffer).unwrap_or("");
                                    match filter_request.filter_method.as_str() {
                                        "changed" => val != old_val,
                                        "unchanged" => val == old_val,
                                        _ => false,
                                    }
                                }
                                "utf-16" => {
                                    let buffer_u16: Vec<u16> = buffer
                                        .clone()
                                        .chunks_exact(2)
                                        .map(|b| u16::from_ne_bytes([b[0], b[1]]))
                                        .collect();
                                    match filter_request.filter_method.as_str() {
                                        "changed" => {
                                            let old_value: Vec<u16> = hex::decode(&value)
                                                .unwrap()
                                                .chunks_exact(2)
                                                .map(|b| u16::from_ne_bytes([b[0], b[1]]))
                                                .collect();
                                            buffer_u16 != old_value
                                        }
                                        "unchanged" => {
                                            let old_value: Vec<u16> = hex::decode(&value)
                                                .unwrap()
                                                .chunks_exact(2)
                                                .map(|b| u16::from_ne_bytes([b[0], b[1]]))
                                                .collect();
                                            buffer_u16 == old_value
                                        }
                                        _ => false,
                                    }
                                }
                                "aob" => match filter_request.filter_method.as_str() {
                                    "changed" => buffer != bytes,
                                    "unchanged" => buffer == bytes,
                                    _ => false,
                                },
                                _ => false,
                            };

                            if pass_filter {
                                found_count.fetch_add(1, Ordering::SeqCst);
                                return Ok(Some((*address, hex::encode(&buffer))));
                            }
                        }
                    }
                    Ok(None)
                })
                .collect();

            match results {
                Ok(results) => {
                    new_positions = results.into_iter().filter_map(|x| x).collect();
                }
                Err(response) => {
                    if do_suspend && is_suspend_success {
                        unsafe {
                            resume_process(pid);
                        }
                    }
                    return Ok(response);
                }
            }
        } else {
            let response = Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::from("Scanid not found"))
                .unwrap();
            return Ok(response);
        }
        if do_suspend && is_suspend_success {
            unsafe {
                resume_process(pid);
            }
        }
        global_positions.insert(filter_request.scan_id.clone(), new_positions.clone());

        if filter_request.return_as_json {
            let limited_positions =
                &new_positions[..std::cmp::min(MAX_RESULTS, new_positions.len())];
            let mut is_rounded: bool;
            let count = found_count.load(Ordering::SeqCst);
            if scan_option.find_type == "unknown" {
                if count > 1_000_000 {
                    is_rounded = true;
                } else {
                    is_rounded = limited_positions.len() != new_positions.len();
                }
            } else {
                is_rounded = limited_positions.len() != new_positions.len();
            }
            let matched_addresses: Vec<serde_json::Value> = limited_positions
                .clone()
                .iter()
                .map(|(address, value)| {
                    json!({
                        "address": address,
                        "value": value
                    })
                })
                .collect();

            let result = json!({
                "matched_addresses": matched_addresses,
                "found":count,
                "is_rounded":is_rounded

            });
            let result_string = result.to_string();
            let response = Response::builder()
                .header("Content-Type", "application/json")
                .body(Body::from(result_string))
                .unwrap();
            Ok(response)
        } else {
            let count = found_count.load(Ordering::SeqCst);
            let result_string = json!({ "found": count }).to_string();
            let response = Response::builder()
                .header("Content-Type", "application/json")
                .body(hyper::Body::from(result_string))
                .unwrap();
            Ok(response)
        }
    } else {
        let response = Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Body::from("Pid not set"))
            .unwrap();
        Ok(response)
    }
}

fn read_process_memory(
    pid: i32,
    address: *mut libc::c_void,
    size: usize,
    buffer: &mut [u8],
) -> Result<isize, Error> {
    let result =
        unsafe { read_memory_native(pid, address as libc::uintptr_t, size, buffer.as_mut_ptr()) };
    if result >= 0 {
        Ok(result as isize)
    } else {
        Err(Error::last_os_error())
    }
}

fn write_process_memory(
    pid: i32,
    address: *mut libc::c_void,
    size: usize,
    buffer: &[u8],
) -> Result<isize, Error> {
    let result =
        unsafe { write_memory_native(pid, address as libc::uintptr_t, size, buffer.as_ptr()) };
    if result >= 0 {
        Ok(result as isize)
    } else {
        Err(Error::last_os_error())
    }
}

#[derive(Serialize)]
struct Region {
    start_address: String,
    end_address: String,
    protection: String,
    file_path: Option<String>,
}

pub async fn enumerate_regions_handler(
    pid_state: Arc<Mutex<Option<i32>>>,
) -> Result<impl warp::Reply, warp::Rejection> {
    let pid = pid_state.lock().unwrap();

    if let Some(pid) = *pid {
        let mut buffer = vec![0u8; 1024 * 1024];

        unsafe {
            enumerate_regions_to_buffer(pid, buffer.as_mut_ptr(), buffer.len());
        }
        let buffer_cstring = unsafe { CString::from_vec_unchecked(buffer) };
        let buffer_string = buffer_cstring.into_string().unwrap();
        let buffer_reader = BufReader::new(buffer_string.as_bytes());

        let mut regions = Vec::new();

        for line in buffer_reader.lines() {
            if let Ok(line) = line {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 {
                    let addresses: Vec<&str> = parts[0].split('-').collect();
                    if addresses.len() == 2 {
                        let region = Region {
                            start_address: addresses[0].to_string(),
                            end_address: addresses[1].to_string(),
                            protection: parts[1].to_string(),
                            file_path: parts.get(2).map(|s| s.to_string()),
                        };
                        regions.push(region);
                    }
                }
            }
        }

        let result = json!({ "regions": regions });
        let result_string = result.to_string();
        let response = Response::builder()
            .header("Content-Type", "application/json")
            .body(hyper::Body::from(result_string))
            .unwrap();
        Ok(response)
    } else {
        let response = Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(hyper::Body::from("Pid not set"))
            .unwrap();
        Ok(response)
    }
}

pub async fn enumerate_process_handler() -> Result<impl Reply, Rejection> {
    let mut count: usize = 0;
    let process_info_ptr = unsafe { enumprocess_native(&mut count) };
    let process_info_slice = unsafe { std::slice::from_raw_parts(process_info_ptr, count) };

    let mut json_array = Vec::new();
    for i in 0..count {
        let process_name = unsafe {
            CStr::from_ptr(process_info_slice[i].processname)
                .to_string_lossy()
                .into_owned()
        };
        json_array.push(json!({
            "pid": process_info_slice[i].pid,
            "processname": process_name
        }));
        unsafe { libc::free(process_info_slice[i].processname as *mut libc::c_void) };
    }

    // for cdylib
    if count == 0 {
        let pid = unsafe { get_pid_native() };
        json_array.push(json!({
            "pid": pid,
            "processname": "self".to_string()
        }));
    } else {
        unsafe {
            libc::free(process_info_ptr as *mut libc::c_void);
        }
    }

    let json_response = warp::reply::json(&json_array);
    Ok(json_response)
}

pub async fn enummodule_handler(
    pid_state: Arc<Mutex<Option<i32>>>,
) -> Result<impl warp::Reply, warp::Rejection> {
    let pid = pid_state.lock().unwrap();
    if let Some(pid) = *pid {
        let mut count: usize = 0;
        let module_info_ptr = unsafe { enummodule_native(pid, &mut count) };
        let module_info_slice = unsafe { std::slice::from_raw_parts(module_info_ptr, count) };

        let mut json_array = Vec::new();
        for i in 0..count {
            let module_name = unsafe {
                CStr::from_ptr(module_info_slice[i].modulename)
                    .to_string_lossy()
                    .into_owned()
            };
            json_array.push(json!({
                "base": module_info_slice[i].base,
                "size": module_info_slice[i].size,
                "is_64bit": module_info_slice[i].is_64bit,
                "modulename": module_name
            }));
            unsafe { libc::free(module_info_slice[i].modulename as *mut libc::c_void) };
        }

        if count == 0 {
            json_array.push(json!({
                "error": format!("No modules found for process with PID: {}", pid)
            }));
        } else {
            unsafe {
                libc::free(module_info_ptr as *mut libc::c_void);
            }
        }

        let result = json!({ "modules": json_array });
        let result_string = result.to_string();
        let response = Response::builder()
            .header("Content-Type", "application/json")
            .body(hyper::Body::from(result_string))
            .unwrap();
        Ok(response)
    } else {
        let response = Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(hyper::Body::from("Pid not set"))
            .unwrap();
        Ok(response)
    }
}

pub fn native_api_init() {
    unsafe {
        native_init();
    }
}
