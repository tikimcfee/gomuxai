//! Stand-ins for the parts of Go's standard library whose behaviour the port
//! must keep word for word. Each module is named for the Go package it
//! replaces and keeps its function names (snake_case).

pub mod filepath;
pub mod json;
pub mod os;
pub mod strconv;
pub mod time;
