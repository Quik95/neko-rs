//! `ext_idle_notify_v1`: knowing when the seat has gone idle.

use wayland_client::globals::GlobalList;
use wayland_client::protocol::wl_seat;
use wayland_client::{Connection, Dispatch, Proxy as _, QueueHandle};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notification_v1::{
    self, ExtIdleNotificationV1,
};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notifier_v1::{
    self, ExtIdleNotifierV1,
};

use crate::state::State;

pub(crate) struct IdleMonitor {
    notification: ExtIdleNotificationV1,
    seat: wl_seat::WlSeat,
}

impl Drop for IdleMonitor {
    fn drop(&mut self) {
        self.notification.destroy();
        if self.seat.version() >= 5 {
            self.seat.release();
        }
    }
}

/// Asks the compositor to say when the seat goes idle.
///
/// Optional on purpose: `ext_idle_notify_v1` is a staging protocol, and an
/// animal that will not start because the compositor lacks it would be a poor
/// trade for a nicety.
pub(crate) fn bind_idle_notification(
    globals: &GlobalList,
    qh: &QueueHandle<State>,
    after: std::time::Duration,
) -> Option<IdleMonitor> {
    // Bind up to v2, but intentionally use `get_idle_notification` rather than
    // v2's `get_input_idle_notification` so idle inhibitors are respected.
    let notifier: ExtIdleNotifierV1 = match globals.bind(qh, 1..=2, ()) {
        Ok(notifier) => notifier,
        Err(error) => {
            log::info!("no ext_idle_notify_v1 ({error}); --idle-notify will do nothing");
            return None;
        }
    };
    // The protocol wants a seat, and idleness is per seat. The first one is the
    // one a single-user desktop has.
    let seat: wl_seat::WlSeat = match globals.bind(qh, 1..=9, ()) {
        Ok(seat) => seat,
        Err(error) => {
            notifier.destroy();
            log::info!("no wl_seat to watch for idleness ({error})");
            return None;
        }
    };
    let millis = u32::try_from(after.as_millis()).unwrap_or(u32::MAX);
    log::info!("sleeping after {millis} ms of seat idleness");
    let notification = notifier.get_idle_notification(millis, &seat, qh, ());
    // Destroying the notifier does not invalidate notifications created from it.
    notifier.destroy();
    Some(IdleMonitor { notification, seat })
}

impl Dispatch<ExtIdleNotifierV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ExtIdleNotifierV1,
        _event: ext_idle_notifier_v1::Event,
        (): &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // The notifier itself has no events.
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &wl_seat::WlSeat,
        _event: wl_seat::Event,
        (): &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // Capabilities and the seat name are of no interest: the surface takes
        // no input, the seat is here only to hang the idle notification on.
    }
}

impl Dispatch<ExtIdleNotificationV1, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &ExtIdleNotificationV1,
        event: ext_idle_notification_v1::Event,
        (): &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_idle_notification_v1::Event::Idled => {
                log::debug!("the seat went idle");
                state.session_idle = true;
            }
            ext_idle_notification_v1::Event::Resumed => {
                log::debug!("the seat came back");
                state.session_idle = false;
            }
            _ => {}
        }
    }
}
