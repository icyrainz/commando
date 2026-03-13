use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use rand::Rng as _;
use tokio::sync::Notify;

pub fn generate_token() -> String {
    let bytes = rand::rng().random::<[u8; 16]>();
    hex::encode(bytes)
}

pub struct StreamExecResult {
    pub exit_code: i32,
    pub duration_ms: u64,
    pub timed_out: bool,
}

pub struct Session {
    pub stdout_buffer: Vec<u8>,
    pub stderr_buffer: Vec<u8>,
    pub completed: bool,
    pub exec_result: Option<StreamExecResult>,
    pub last_polled: Instant,
    pub notify: Rc<Notify>,
    pub rpc_task: Option<tokio::task::JoinHandle<()>>,
}

impl Session {
    fn new() -> Self {
        Self {
            stdout_buffer: Vec::new(),
            stderr_buffer: Vec::new(),
            completed: false,
            exec_result: None,
            last_polled: Instant::now(),
            notify: Rc::new(Notify::new()),
            rpc_task: None,
        }
    }

    pub fn drain_stdout(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.stdout_buffer)
    }

    pub fn drain_stderr(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.stderr_buffer)
    }

    pub fn drain_stdout_up_to(&mut self, max_bytes: usize) -> Vec<u8> {
        if self.stdout_buffer.len() <= max_bytes {
            std::mem::take(&mut self.stdout_buffer)
        } else {
            let remainder = self.stdout_buffer.split_off(max_bytes);
            std::mem::replace(&mut self.stdout_buffer, remainder)
        }
    }

    pub fn drain_stderr_up_to(&mut self, max_bytes: usize) -> Vec<u8> {
        if self.stderr_buffer.len() <= max_bytes {
            std::mem::take(&mut self.stderr_buffer)
        } else {
            let remainder = self.stderr_buffer.split_off(max_bytes);
            std::mem::replace(&mut self.stderr_buffer, remainder)
        }
    }

    pub fn touch(&mut self) {
        self.last_polled = Instant::now();
    }

    pub fn total_buffered(&self) -> usize {
        self.stdout_buffer.len() + self.stderr_buffer.len()
    }
}

pub struct SessionMap {
    sessions: HashMap<String, Session>,
    token_to_session: HashMap<String, String>,
}

impl SessionMap {
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            token_to_session: HashMap::new(),
        }
    }

    pub fn create_session(&mut self) -> (String, String) {
        let token = generate_token();
        let session_id = generate_token();
        self.sessions.insert(session_id.clone(), Session::new());
        self.token_to_session
            .insert(token.clone(), session_id.clone());
        (token, session_id)
    }

    pub fn get_by_token(&self, token: &str) -> Option<&Session> {
        let session_id = self.token_to_session.get(token)?;
        self.sessions.get(session_id)
    }

    pub fn get_by_token_mut(&mut self, token: &str) -> Option<&mut Session> {
        let session_id = self.token_to_session.get(token)?.clone();
        self.sessions.get_mut(&session_id)
    }

    pub fn rotate_token(&mut self, old_token: &str) -> Option<String> {
        let session_id = self.token_to_session.remove(old_token)?;
        let new_token = generate_token();
        self.token_to_session.insert(new_token.clone(), session_id);
        Some(new_token)
    }

    pub fn remove_by_token(&mut self, token: &str) -> Option<Session> {
        let session_id = self.token_to_session.remove(token)?;
        self.sessions.remove(&session_id)
    }

    pub fn get_by_id_mut(&mut self, session_id: &str) -> Option<&mut Session> {
        self.sessions.get_mut(session_id)
    }

    pub fn session_id_for_token(&self, token: &str) -> Option<&str> {
        self.token_to_session.get(token).map(String::as_str)
    }

    pub fn cleanup_expired(&mut self, idle_timeout: Duration) -> Vec<String> {
        let now = Instant::now();
        let expired_ids: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, session)| now.duration_since(session.last_polled) >= idle_timeout)
            .map(|(id, _)| id.clone())
            .collect();

        for session_id in &expired_ids {
            if let Some(mut session) = self.sessions.remove(session_id)
                && let Some(task) = session.rpc_task.take()
            {
                task.abort();
            }
        }

        // Remove all token mappings pointing to expired session IDs
        self.token_to_session
            .retain(|_, sid| !expired_ids.contains(sid));

        expired_ids
    }
}

impl Default for SessionMap {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_map_create_and_lookup() {
        let mut map = SessionMap::new();
        let (token, _) = map.create_session();
        assert!(map.get_by_token(&token).is_some());
        assert!(map.get_by_token("bogus").is_none());
    }

    #[test]
    fn session_token_rotation() {
        let mut map = SessionMap::new();
        let (token1, _) = map.create_session();
        let token2 = map.rotate_token(&token1).unwrap();
        assert_ne!(token1, token2);
        assert!(map.get_by_token(&token1).is_none());
        assert!(map.get_by_token(&token2).is_some());
    }

    #[test]
    fn session_drain_stdout() {
        let mut map = SessionMap::new();
        let (token, _) = map.create_session();
        map.get_by_token_mut(&token)
            .unwrap()
            .stdout_buffer
            .extend_from_slice(b"hello world");
        let drained = map.get_by_token_mut(&token).unwrap().drain_stdout();
        assert_eq!(drained, b"hello world");
        assert!(
            map.get_by_token_mut(&token)
                .unwrap()
                .stdout_buffer
                .is_empty()
        );
    }

    #[test]
    fn session_drain_stderr() {
        let mut map = SessionMap::new();
        let (token, _) = map.create_session();
        map.get_by_token_mut(&token)
            .unwrap()
            .stderr_buffer
            .extend_from_slice(b"err msg");
        let drained = map.get_by_token_mut(&token).unwrap().drain_stderr();
        assert_eq!(drained, b"err msg");
    }

    #[test]
    fn session_cleanup_expired() {
        let mut map = SessionMap::new();
        let (token, _) = map.create_session();
        map.get_by_token_mut(&token).unwrap().last_polled =
            Instant::now() - Duration::from_secs(120);
        let expired = map.cleanup_expired(Duration::from_secs(60));
        assert_eq!(expired.len(), 1);
        assert!(map.get_by_token(&token).is_none());
    }

    #[test]
    fn generate_token_is_unique() {
        let t1 = generate_token();
        let t2 = generate_token();
        assert_ne!(t1, t2);
        assert_eq!(t1.len(), 32); // 128-bit = 16 bytes = 32 hex chars
    }

    #[test]
    fn session_drain_up_to() {
        let mut map = SessionMap::new();
        let (token, _) = map.create_session();
        let session = map.get_by_token_mut(&token).unwrap();
        session
            .stdout_buffer
            .extend_from_slice(b"hello world, this is a longer message");
        let drained = session.drain_stdout_up_to(11);
        assert_eq!(drained, b"hello world");
        assert_eq!(session.stdout_buffer, b", this is a longer message");
    }
}
