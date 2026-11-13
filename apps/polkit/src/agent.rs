//! The D-Bus side: registering with polkitd as this session's authentication agent, and
//! taking its requests.
//!
//! polkitd calls `BeginAuthentication` when an app in this session asks for something that
//! needs a password, and waits for the call to return: success once the helper has accepted
//! a password, `org.freedesktop.PolicyKit1.Error.Cancelled` if the user gave up. It may call
//! `CancelAuthentication` in the meantime, for instance when the app quits.

use std::{collections::HashMap, sync::Mutex};

use smithay_client_toolkit::reexports::calloop::channel::Sender;
use zbus::zvariant::{OwnedValue, Value};

const PATH: &str = "/org/kiln/PolicyKit1/AuthenticationAgent";

/// Someone who can give the password: polkit asks for one of these.
#[derive(Debug, Clone, PartialEq)]
pub struct User {
    pub uid: u32,
    pub login: String,
    /// The full name if there is one.
    pub label: String,
}

pub struct Begin {
    pub action_id: String,
    pub message: String,
    pub cookie: String,
    /// Who may answer, the user running this first if they're among them.
    pub users: Vec<User>,
    pub reply: async_channel::Sender<Result<(), AgentError>>,
}

pub enum Request {
    Begin(Begin),
    Cancel(String),
}

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.freedesktop.PolicyKit1.Error")]
pub enum AgentError {
    #[zbus(error)]
    ZBus(zbus::Error),
    Failed(String),
    Cancelled(String),
}

struct Agent {
    requests: Mutex<Sender<Request>>,
}

#[zbus::interface(name = "org.freedesktop.PolicyKit1.AuthenticationAgent")]
impl Agent {
    #[allow(clippy::too_many_arguments)]
    async fn begin_authentication(
        &self,
        action_id: String,
        message: String,
        _icon_name: String,
        _details: HashMap<String, String>,
        cookie: String,
        identities: Vec<(String, HashMap<String, OwnedValue>)>,
    ) -> Result<(), AgentError> {
        tracing::info!("password asked for {action_id}");
        let users = users(&identities);
        if users.is_empty() {
            return Err(AgentError::Failed("nobody here can give a password for this".into()));
        }
        let (reply, answer) = async_channel::bounded(1);
        let begin = Begin {
            action_id,
            message,
            cookie,
            users,
            reply,
        };
        let sent = match self.requests.lock() {
            Ok(r) => r.send(Request::Begin(begin)).is_ok(),
            Err(_) => false,
        };
        if !sent {
            return Err(AgentError::Failed("the agent is shutting down".into()));
        }
        answer
            .recv()
            .await
            .unwrap_or_else(|_| Err(AgentError::Cancelled("the dialog went away".into())))
    }

    async fn cancel_authentication(&self, cookie: String) {
        if let Ok(r) = self.requests.lock() {
            let _ = r.send(Request::Cancel(cookie));
        }
    }
}

/// Serve the agent on the system bus and register it for this session. The connection must
/// be kept for as long as the agent runs.
pub fn register(requests: Sender<Request>) -> Result<zbus::blocking::Connection, String> {
    let conn = zbus::blocking::Connection::system().map_err(|e| format!("can't reach the system bus: {e}"))?;
    conn.object_server()
        .at(
            PATH,
            Agent {
                requests: Mutex::new(requests),
            },
        )
        .map_err(|e| e.to_string())?;
    let session = session_id(&conn)?;
    let mut details: HashMap<&str, Value> = HashMap::new();
    details.insert("session-id", Value::from(session.as_str()));
    let subject = ("unix-session", details);
    conn.call_method(
        Some("org.freedesktop.PolicyKit1"),
        "/org/freedesktop/PolicyKit1/Authority",
        Some("org.freedesktop.PolicyKit1.Authority"),
        "RegisterAuthenticationAgent",
        &(subject, locale(), PATH),
    )
    .map_err(|e| match e {
        zbus::Error::MethodError(_, Some(msg), _) => msg,
        e => e.to_string(),
    })?;
    tracing::info!("registered as the password agent for session {session}");
    Ok(conn)
}

/// The login session we run in: `$XDG_SESSION_ID`, or logind's answer.
fn session_id(conn: &zbus::blocking::Connection) -> Result<String, String> {
    if let Ok(id) = std::env::var("XDG_SESSION_ID")
        && !id.is_empty()
    {
        return Ok(id);
    }
    let manager = |method: &str, arg: u32| -> zbus::Result<zbus::zvariant::OwnedObjectPath> {
        conn.call_method(
            Some("org.freedesktop.login1"),
            "/org/freedesktop/login1",
            Some("org.freedesktop.login1.Manager"),
            method,
            &arg,
        )?
        .body()
        .deserialize()
    };
    let property = |path: &str, iface: &str, name: &str| -> zbus::Result<OwnedValue> {
        let reply = conn.call_method(
            Some("org.freedesktop.login1"),
            path,
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &(iface, name),
        )?;
        reply.body().deserialize::<OwnedValue>()
    };
    // Our own process's session, or else the user's main one (when started outside it).
    if let Ok(path) = manager("GetSessionByPID", std::process::id())
        && let Ok(id) = property(path.as_str(), "org.freedesktop.login1.Session", "Id")
        && let Ok(id) = String::try_from(id)
    {
        return Ok(id);
    }
    // SAFETY: getuid can't fail.
    let uid = unsafe { libc::getuid() };
    let user = manager("GetUser", uid).map_err(|e| format!("can't find the login session: {e}"))?;
    let display = property(user.as_str(), "org.freedesktop.login1.User", "Display")
        .map_err(|e| format!("can't find the login session: {e}"))?;
    let (id, _): (String, zbus::zvariant::OwnedObjectPath) = display
        .try_into()
        .map_err(|e| format!("can't find the login session: {e}"))?;
    if id.is_empty() {
        return Err("not in a login session".into());
    }
    Ok(id)
}

fn locale() -> String {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|v| std::env::var(v).ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "C".into())
}

/// The people polkit will take a password from: users as they are, groups as their members.
/// The user running this comes first.
fn users(identities: &[(String, HashMap<String, OwnedValue>)]) -> Vec<User> {
    let mut uids: Vec<u32> = Vec::new();
    for (kind, details) in identities {
        let number = |key: &str| details.get(key).and_then(|v| u32::try_from(v).ok());
        match kind.as_str() {
            "unix-user" => uids.extend(number("uid")),
            "unix-group" => {
                if let Some(gid) = number("gid") {
                    uids.extend(group_members(gid));
                }
            }
            _ => {}
        }
    }
    // SAFETY: getuid can't fail.
    let me = unsafe { libc::getuid() };
    let mut seen = Vec::new();
    let mut out: Vec<User> = uids
        .into_iter()
        .filter(|u| {
            let new = !seen.contains(u);
            seen.push(*u);
            new
        })
        .filter_map(user)
        .collect();
    out.sort_by_key(|u| (u.uid != me, u.uid == 0));
    out
}

fn user(uid: u32) -> Option<User> {
    // SAFETY: getpwuid_r writes into our own buffers; the strings are copied out before they go.
    unsafe {
        let mut pw: libc::passwd = std::mem::zeroed();
        let mut buf = vec![0 as libc::c_char; 16 * 1024];
        let mut found = std::ptr::null_mut();
        libc::getpwuid_r(uid, &mut pw, buf.as_mut_ptr(), buf.len(), &mut found);
        if found.is_null() {
            return None;
        }
        let login = std::ffi::CStr::from_ptr(pw.pw_name).to_string_lossy().into_owned();
        let gecos = if pw.pw_gecos.is_null() {
            String::new()
        } else {
            std::ffi::CStr::from_ptr(pw.pw_gecos).to_string_lossy().into_owned()
        };
        let full = gecos.split(',').next().unwrap_or("").trim().to_string();
        let label = if full.is_empty() { login.clone() } else { full };
        Some(User { uid, login, label })
    }
}

/// Users in a group: those listed in it, and those with it as their main group.
fn group_members(gid: u32) -> Vec<u32> {
    let mut names: Vec<String> = Vec::new();
    // SAFETY: getgrgid_r writes into our own buffers; the names are copied out.
    unsafe {
        let mut gr: libc::group = std::mem::zeroed();
        let mut buf = vec![0 as libc::c_char; 64 * 1024];
        let mut found = std::ptr::null_mut();
        libc::getgrgid_r(gid, &mut gr, buf.as_mut_ptr(), buf.len(), &mut found);
        if !found.is_null() && !gr.gr_mem.is_null() {
            let mut p = gr.gr_mem;
            while !(*p).is_null() {
                names.push(std::ffi::CStr::from_ptr(*p).to_string_lossy().into_owned());
                p = p.add(1);
            }
        }
    }
    let mut uids: Vec<u32> = names.iter().filter_map(|n| uid_of(n)).collect();
    // Those with it as their main group aren't listed in it; /etc/passwd has them.
    if let Ok(passwd) = std::fs::read_to_string("/etc/passwd") {
        for line in passwd.lines() {
            let f: Vec<&str> = line.split(':').collect();
            if f.len() > 3
                && f[3].parse() == Ok(gid)
                && let Ok(uid) = f[2].parse()
            {
                uids.push(uid);
            }
        }
    }
    uids
}

fn uid_of(name: &str) -> Option<u32> {
    let name = std::ffi::CString::new(name).ok()?;
    // SAFETY: as in `user`.
    unsafe {
        let mut pw: libc::passwd = std::mem::zeroed();
        let mut buf = vec![0 as libc::c_char; 16 * 1024];
        let mut found = std::ptr::null_mut();
        libc::getpwnam_r(name.as_ptr(), &mut pw, buf.as_mut_ptr(), buf.len(), &mut found);
        (!found.is_null()).then_some(pw.pw_uid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identities_become_users() {
        // SAFETY: getuid can't fail.
        let me = unsafe { libc::getuid() };
        let id = |kind: &str, key: &str, n: u32| {
            let mut m = HashMap::new();
            m.insert(key.to_string(), OwnedValue::from(n));
            (kind.to_string(), m)
        };
        let got = users(&[
            id("unix-user", "uid", 0),
            id("unix-user", "uid", me),
            id("unix-user", "uid", me),
        ]);
        assert_eq!(got.len(), if me == 0 { 1 } else { 2 }, "{got:?}");
        assert_eq!(got[0].uid, me, "the user running this comes first");
        let root = users(&[id("unix-group", "gid", 0)]);
        assert!(root.iter().any(|u| u.uid == 0), "root's main group is 0: {root:?}");
    }
}
