use crate::platform::wide;
use std::{
    ffi::c_void,
    ptr::{null, null_mut},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Networking::WinHttp::*,
    Security::Cryptography::{BCRYPT_SHA256_ALG_HANDLE, BCryptHash},
};

struct HttpHandle(*mut c_void);

impl HttpHandle {
    fn from_raw(handle: *mut c_void) -> Option<Self> {
        (!handle.is_null()).then_some(Self(handle))
    }
}

impl Drop for HttpHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { WinHttpCloseHandle(self.0) };
        }
    }
}

pub fn session() -> Option<Session> {
    let session = HttpHandle::from_raw(unsafe {
        WinHttpOpen(
            wide(concat!("SlimStore/", env!("CARGO_PKG_VERSION"))).as_ptr(),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            null(),
            null(),
            0,
        )
    })?;
    if unsafe { WinHttpSetTimeouts(session.0, 2_000, 2_000, 2_000, 3_000) } == 0 {
        return None;
    }
    Some(Session(session))
}

pub fn sha256(bytes: &[u8]) -> Option<[u8; 32]> {
    let mut digest = [0u8; 32];
    let status = unsafe {
        BCryptHash(
            BCRYPT_SHA256_ALG_HANDLE,
            null(),
            0,
            bytes.as_ptr(),
            bytes.len() as u32,
            digest.as_mut_ptr(),
            digest.len() as u32,
        )
    };
    (status == 0).then_some(digest)
}

pub fn sha256_matches(bytes: &[u8], expected: &str) -> bool {
    expected.len() == 64
        && sha256(bytes).is_some_and(|digest| {
            digest
                .iter()
                .zip(expected.as_bytes().as_chunks::<2>().0)
                .all(|(actual, pair)| {
                    std::str::from_utf8(pair)
                        .ok()
                        .and_then(|hex| u8::from_str_radix(hex, 16).ok())
                        == Some(*actual)
                })
        })
}

pub struct Session(HttpHandle);

impl Session {
    pub fn get(
        &self,
        host: &str,
        path: &str,
        limit: usize,
        enabled: impl Fn() -> bool,
    ) -> Option<Vec<u8>> {
        if !enabled() {
            return None;
        }
        let server =
            HttpHandle::from_raw(unsafe { WinHttpConnect(self.0.0, wide(host).as_ptr(), 443, 0) })?;
        let request = HttpHandle::from_raw(unsafe {
            WinHttpOpenRequest(
                server.0,
                wide("GET").as_ptr(),
                wide(path).as_ptr(),
                null(),
                null(),
                null(),
                WINHTTP_FLAG_SECURE,
            )
        })?;
        let disabled =
            WINHTTP_DISABLE_REDIRECTS | WINHTTP_DISABLE_COOKIES | WINHTTP_DISABLE_AUTHENTICATION;
        let mut status = 0u32;
        let mut size = size_of::<u32>() as u32;
        let start = Instant::now();
        // WinHTTP still enforces TLS certificate validation. No alternate URLs or credentials are used.
        if !enabled()
            || unsafe {
                WinHttpSetOption(
                    request.0,
                    WINHTTP_OPTION_DISABLE_FEATURE,
                    (&raw const disabled).cast(),
                    size,
                ) == 0
                    || WinHttpSendRequest(request.0, null(), 0, null(), 0, 0, 0) == 0
                    || WinHttpReceiveResponse(request.0, null_mut()) == 0
                    || WinHttpQueryHeaders(
                        request.0,
                        WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                        null(),
                        (&raw mut status).cast(),
                        &mut size,
                        null_mut(),
                    ) == 0
            }
            || status != 200
        {
            return None;
        }

        let mut content_length = 0u32;
        size = size_of::<u32>() as u32;
        if unsafe {
            WinHttpQueryHeaders(
                request.0,
                WINHTTP_QUERY_CONTENT_LENGTH | WINHTTP_QUERY_FLAG_NUMBER,
                null(),
                (&raw mut content_length).cast(),
                &mut size,
                null_mut(),
            )
        } != 0
            && content_length as usize > limit
        {
            return None;
        }
        let mut bytes = Vec::new();
        let mut block = [0u8; 8192];
        while enabled() && start.elapsed() < Duration::from_secs(8) {
            let mut read = 0;
            if unsafe {
                WinHttpReadData(
                    request.0,
                    block.as_mut_ptr().cast(),
                    block.len() as u32,
                    &mut read,
                )
            } == 0
                || bytes.len() + read as usize > limit
            {
                return None;
            }
            if read == 0 {
                return Some(bytes);
            }
            bytes.extend_from_slice(&block[..read as usize]);
        }
        None
    }
}
