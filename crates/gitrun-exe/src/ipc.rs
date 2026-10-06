//! Wire protocol for workflow Git*Run requests.
//!
//! Requests cross a local protected GitRun socket. The bearer token is
//! runner-scoped; GSR binds it to the managed runner before authorization.
//! Responses are framed as newline-delimited JSON events so handlers can
//! later stream stdout/stderr without changing the transport.

use crate::{ApiInvocation, ExecutionEvent};
use serde::{Deserialize, Serialize};

pub const DEFAULT_SOCKET_PATH: &str = "/run/gitrun/api.sock";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireRequest {
    pub invocation: ApiInvocation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WireResponse {
    Accepted,
    Error { message: String },
    Event(ExecutionEvent),
}

#[cfg(test)]
mod tests {
    use super::*;
    use gitrun_core::{GitRunApi, GitRunOperation};
    use std::collections::BTreeMap;

    #[test]
    fn wire_request_round_trips() {
        let request = WireRequest {
            invocation: ApiInvocation {
                api: GitRunApi::GitStatusRun,
                operation: GitRunOperation::Status,
                repository: "owner/repo".into(),
                workflow: "ci.yml".into(),
                run_id: Some(1),
                job: "build".into(),
                runner: "runner-1".into(),
                resource: None,
                arguments: BTreeMap::new(),
            },
        };
        let encoded = serde_json::to_string(&request).unwrap();
        let decoded: WireRequest = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, request);
    }
}
