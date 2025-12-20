// Copyright 2023 System76 <info@system76.com>
// SPDX-License-Identifier: GPL-3.0-only

mod app;
mod config;
mod localize;
mod device;
mod worker;
mod agent;

use crate::localize::localize;

#[inline]
pub fn run() -> cosmic::iced::Result {
    localize();
    app::run()
}
