//! Checking the password with PAM, the same way logging in does.
//!
//! Uses the PAM service `kiln-lock` (installed to /etc/pam.d by install.sh), or
//! `login` when that file is missing. A wrong password usually takes a couple of seconds to
//! be turned down (PAM's failure delay), so this runs on its own thread.

use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    path::Path,
    ptr,
};

use crate::password::Password;

const SERVICE: &str = "kiln-lock";

const PAM_SUCCESS: c_int = 0;
const PAM_PROMPT_ECHO_OFF: c_int = 1;
const PAM_PROMPT_ECHO_ON: c_int = 2;
const PAM_ERROR_MSG: c_int = 3;
const PAM_TEXT_INFO: c_int = 4;
const PAM_BUF_ERR: c_int = 5;
const PAM_CONV_ERR: c_int = 19;
const PAM_REFRESH_CRED: c_int = 0x0010;
const PAM_MAX_NUM_MSG: c_int = 32;

#[repr(C)]
struct PamMessage {
    msg_style: c_int,
    msg: *const c_char,
}

#[repr(C)]
struct PamResponse {
    resp: *mut c_char,
    resp_retcode: c_int,
}

type ConvFn = extern "C" fn(c_int, *mut *const PamMessage, *mut *mut PamResponse, *mut c_void) -> c_int;

#[repr(C)]
struct PamConv {
    conv: ConvFn,
    appdata_ptr: *mut c_void,
}

#[link(name = "pam")]
unsafe extern "C" {
    fn pam_start(service: *const c_char, user: *const c_char, conv: *const PamConv, pamh: *mut *mut c_void) -> c_int;
    fn pam_authenticate(pamh: *mut c_void, flags: c_int) -> c_int;
    fn pam_setcred(pamh: *mut c_void, flags: c_int) -> c_int;
    fn pam_end(pamh: *mut c_void, status: c_int) -> c_int;
    fn pam_strerror(pamh: *mut c_void, errnum: c_int) -> *const c_char;
}

pub enum Outcome {
    Unlock,
    /// Turned down, with what PAM said if anything (e.g. "account locked for 10 minutes").
    Denied(Option<String>),
}

/// What the conversation function needs: the password to answer prompts with, and a place
/// for messages PAM wants shown.
struct Conversation {
    /// The password, nul-terminated.
    secret: *const c_char,
    messages: Vec<String>,
}

extern "C" fn converse(
    count: c_int,
    msgs: *mut *const PamMessage,
    out: *mut *mut PamResponse,
    data: *mut c_void,
) -> c_int {
    if !(1..=PAM_MAX_NUM_MSG).contains(&count) || msgs.is_null() || out.is_null() || data.is_null() {
        return PAM_CONV_ERR;
    }
    // SAFETY: PAM hands back the pointer we gave pam_start, and `count` valid messages.
    // Responses are malloc'd because PAM frees them (after wiping them).
    unsafe {
        let conv = &mut *(data as *mut Conversation);
        let replies = libc::calloc(count as usize, size_of::<PamResponse>()) as *mut PamResponse;
        if replies.is_null() {
            return PAM_BUF_ERR;
        }
        for i in 0..count as usize {
            let msg = &**msgs.add(i);
            let text = || (!msg.msg.is_null()).then(|| CStr::from_ptr(msg.msg).to_string_lossy().trim().to_string());
            match msg.msg_style {
                PAM_PROMPT_ECHO_OFF | PAM_PROMPT_ECHO_ON => {
                    let copy = libc::strdup(conv.secret);
                    if copy.is_null() {
                        for j in 0..i {
                            libc::free((*replies.add(j)).resp.cast());
                        }
                        libc::free(replies.cast());
                        return PAM_BUF_ERR;
                    }
                    (*replies.add(i)).resp = copy;
                }
                PAM_ERROR_MSG | PAM_TEXT_INFO => {
                    if let Some(t) = text().filter(|t| !t.is_empty()) {
                        tracing::info!("pam: {t}");
                        conv.messages.push(t);
                    }
                }
                _ => {}
            }
        }
        *out = replies;
    }
    PAM_SUCCESS
}

fn service() -> &'static str {
    let installed = ["/etc/pam.d", "/usr/lib/pam.d", "/usr/local/etc/pam.d"]
        .iter()
        .any(|dir| Path::new(dir).join(SERVICE).is_file());
    if installed {
        SERVICE
    } else {
        tracing::warn!("/etc/pam.d/{SERVICE} is missing (install.sh adds it); checking passwords as \"login\"");
        "login"
    }
}

/// Check `password` for `user`. Blocks, often for seconds when it's wrong.
pub fn check(user: &str, password: Password) -> Outcome {
    let (Ok(service), Ok(user)) = (CString::new(service()), CString::new(user)) else {
        return Outcome::Denied(None);
    };
    // A nul-terminated copy for PAM, wiped below. Built in a buffer that never reallocates.
    let mut secret = Vec::with_capacity(password.as_str().len() + 1);
    secret.extend_from_slice(password.as_str().as_bytes());
    secret.push(0);
    drop(password);
    if secret[..secret.len() - 1].contains(&0) {
        wipe(&mut secret);
        return Outcome::Denied(None);
    }

    let mut conversation = Conversation {
        secret: secret.as_ptr().cast(),
        messages: Vec::new(),
    };
    let conv = PamConv {
        conv: converse,
        appdata_ptr: (&mut conversation as *mut Conversation).cast(),
    };
    let mut handle = ptr::null_mut();
    // SAFETY: every pointer stays valid until pam_end, which is always called on a handle.
    let result = unsafe {
        let started = pam_start(service.as_ptr(), user.as_ptr(), &conv, &mut handle);
        if started != PAM_SUCCESS {
            tracing::error!("pam_start failed: {started}");
            wipe(&mut secret);
            return Outcome::Denied(Some("Can't check passwords (PAM failed to start)".into()));
        }
        let result = pam_authenticate(handle, 0);
        if result == PAM_SUCCESS {
            // Renew credentials like Kerberos tickets; unlocking doesn't depend on it.
            pam_setcred(handle, PAM_REFRESH_CRED);
        } else {
            let why = pam_strerror(handle, result);
            if !why.is_null() {
                tracing::info!("password turned down: {}", CStr::from_ptr(why).to_string_lossy());
            }
        }
        pam_end(handle, result);
        result
    };
    wipe(&mut secret);
    if result == PAM_SUCCESS {
        Outcome::Unlock
    } else {
        Outcome::Denied(conversation.messages.pop())
    }
}

fn wipe(bytes: &mut [u8]) {
    for b in bytes.iter_mut() {
        // SAFETY: a valid &mut u8; volatile so the wipe isn't optimized away.
        unsafe { ptr::write_volatile(b, 0) };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}
