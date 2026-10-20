// SPDX-FileCopyrightText: 2026 Reion Wong <reionwong@gmail.com>
// SPDX-License-Identifier: BSD-3-Clause

// The compositor's cutefish_computer_control_v1, kept in sync with
// cutefish-compositor/protocols by copy.

#![allow(dead_code, non_camel_case_types, unused_unsafe, unused_variables)]
#![allow(non_upper_case_globals, non_snake_case, unused_imports)]
#![allow(missing_docs, clippy::all)]

use wayland_client;
use wayland_client::protocol::*;

pub mod __interfaces {
    use wayland_client::protocol::__interfaces::*;
    wayland_scanner::generate_interfaces!("protocols/cutefish-computer-control-v1.xml");
}
use self::__interfaces::*;

wayland_scanner::generate_client_code!("protocols/cutefish-computer-control-v1.xml");
