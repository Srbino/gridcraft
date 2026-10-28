//! Fail-closed private-port access state. Replies describe applied native state.

pub fn initial_read_only(port: bool, query: Option<&str>) -> bool {
    port || query == Some("1")
}

pub struct Request {
    pub request_id: String,
    pub read_only: bool,
}

pub struct Reply {
    pub request_id: String,
    pub read_only: bool,
}

pub struct AccessState {
    read_only: bool,
    pending: Option<Request>,
}

impl Default for AccessState {
    fn default() -> Self {
        Self { read_only: true, pending: None }
    }
}

impl AccessState {
    pub fn read_only(&self) -> bool {
        self.read_only
    }

    pub fn request(&mut self, request_id: String, read_only: bool) -> Result<(), String> {
        if request_id.is_empty() || request_id.len() > 128 {
            return Err("An access request id of at most 128 bytes is required.".into());
        }
        if self.pending.is_some() {
            return Err("Another access request is pending.".into());
        }
        self.pending = Some(Request { request_id, read_only });
        Ok(())
    }

    pub fn take(&mut self) -> Option<Request> {
        self.pending.take()
    }

    pub fn applied(&mut self, request: Request, actual: bool) -> Reply {
        self.read_only = actual;
        Reply { request_id: request.request_id, read_only: actual }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn native_port_boot_is_read_only_even_when_the_query_is_absent_or_false() {
        for query in [None, Some("0"), Some("1"), Some("false"), Some("")] {
            assert!(super::initial_read_only(true, query));
        }
        assert!(!super::initial_read_only(false, None));
        assert!(!super::initial_read_only(false, Some("0")));
        assert!(super::initial_read_only(false, Some("1")));
    }

    #[test]
    fn access_request_never_claims_application_before_the_guard_actually_changes() {
        let mut access = super::AccessState::default();
        assert!(access.read_only());
        access.request("lease-proof".into(), false).unwrap();
        assert!(access.read_only());
        let request = access.take().unwrap();
        assert!(!request.read_only);
        assert!(access.read_only());
        let reply = access.applied(request, false);
        assert_eq!(reply.request_id, "lease-proof");
        assert!(!reply.read_only);
        assert!(!access.read_only());
        access.request("revoke".into(), true).unwrap();
        let request = access.take().unwrap();
        assert!(request.read_only);
        let reply = access.applied(request, true);
        assert!(reply.read_only);
        assert!(access.read_only());
        assert!(access.take().is_none());
    }

    #[test]
    fn access_queue_is_bounded_and_replies_report_actual_instead_of_requested_state() {
        let mut access = super::AccessState::default();
        assert!(access.request(String::new(), false).is_err());
        assert!(access.request("x".repeat(129), false).is_err());
        access.request("first".into(), false).unwrap();
        assert!(access.request("overlap".into(), true).is_err());
        let request = access.take().unwrap();
        assert!(!request.read_only);
        let reply = access.applied(request, true);
        assert_eq!(reply.request_id, "first");
        assert!(reply.read_only);
        assert!(access.read_only());
    }
}
