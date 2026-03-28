/*
Copyright 2022 The Kuasar Authors.

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

/// Errors produced by `vmm-engine`.
#[derive(Debug)]
pub enum Error {
    /// A sandbox with the given id already exists.
    AlreadyExists(String),
    /// No sandbox found with the given id.
    NotFound(String),
    /// The requested state transition is not valid.
    InvalidState(String),
    /// An operation timed out.
    Timeout(String),
    /// Any other error (wraps `anyhow::Error` for propagation).
    Other(anyhow::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::AlreadyExists(id) => write!(f, "sandbox already exists: {}", id),
            Error::NotFound(id) => write!(f, "sandbox not found: {}", id),
            Error::InvalidState(msg) => write!(f, "invalid state: {}", msg),
            Error::Timeout(msg) => write!(f, "timeout: {}", msg),
            Error::Other(e) => write!(f, "{}", e),
        }
    }
}

impl std::error::Error for Error {}

impl From<anyhow::Error> for Error {
    fn from(e: anyhow::Error) -> Self {
        Error::Other(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
