use super::ui::{Ui, UiEvent};

use aperturec_client::{CancellationToken, Connection, Draw, Event as ServerEvent, QuitReason};
use aperturec_utils::channels::SenderExt;

use async_channel::{Receiver, Sender, unbounded};
use gtk4::{self as gtk, glib, prelude::*};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::thread;
use tracing::*;

fn spawn_server_event_thread(
    conn: Arc<Connection>,
    cancel_token: CancellationToken,
    control_tx: Sender<ServerEvent>,
    draw_tx: Sender<Draw>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let _s = trace_span!("main-server-event-thread").entered();
        debug!("server event thread started");
        loop {
            match conn.wait_event_or_cancel(&cancel_token) {
                Ok(Some(event)) => {
                    trace!(?event, "received server event");
                    let forward_failed = match event {
                        ServerEvent::Draw(draw) => draw_tx.send_blocking(draw).is_err(),
                        event => control_tx.send_blocking(event).is_err(),
                    };
                    if forward_failed {
                        warn!("forwarding server event failed; shutting down event thread");
                        break;
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    warn!(%error, "failed getting server event");
                    control_tx.send_or_warn(ServerEvent::Quit(QuitReason::UnrecoverableError(
                        Box::new(error),
                    )));
                    break;
                }
            }
        }
        debug!("server event thread exiting");
    })
}

fn spawn_control_task(
    ui: Rc<RefCell<Ui>>,
    ui_event_tx: Sender<UiEvent>,
    control_rx: Receiver<ServerEvent>,
) -> glib::JoinHandle<()> {
    glib::MainContext::default().spawn_local_with_priority(
        glib::Priority::DEFAULT,
        glib::clone!(
            #[strong]
            ui,
            #[strong(rename_to = tx)]
            ui_event_tx,
            async move {
                debug!("control task started");
                while let Ok(event) = control_rx.recv().await {
                    trace!(?event, "control event received");
                    let mut ui = ui.borrow_mut();
                    match event {
                        ServerEvent::CursorChange(cursor) => ui.change_cursor(cursor),
                        ServerEvent::DisplayChange(dc) => {
                            if let Err(error) = ui.set_display_configuration(dc) {
                                warn!(%error, "changing display configuration");
                            }
                        }
                        ServerEvent::Quit(quit_reason) => {
                            debug!(?quit_reason, "server requested quit");
                            ui.show_exit_reason(quit_reason);
                            tx.send_or_warn(UiEvent::Shutdown);
                            break;
                        }
                        ServerEvent::Draw(_) => {
                            warn!("draw event on control channel; dropping");
                        }
                    }
                }
                debug!("control task exiting");
                tx.send_or_warn(UiEvent::Shutdown);
            }
        ),
    )
}

fn spawn_draw_task(ui: Rc<RefCell<Ui>>, draw_rx: Receiver<Draw>) -> glib::JoinHandle<()> {
    glib::MainContext::default().spawn_local_with_priority(
        glib::Priority::DEFAULT_IDLE,
        glib::clone!(
            #[strong]
            ui,
            async move {
                trace!("draw task started");
                while let Ok(draw) = draw_rx.recv().await {
                    trace!(frame = draw.frame, "draw event received");
                    ui.borrow_mut().draw(draw);
                }
                trace!("draw task exiting");
            }
        ),
    )
}

fn spawn_ui_task(
    ui: Rc<RefCell<Ui>>,
    ui_event_rx: Receiver<UiEvent>,
    app_weak: glib::WeakRef<gtk::Application>,
) -> glib::JoinHandle<()> {
    glib::MainContext::default().spawn_local(glib::clone!(
        #[strong]
        ui,
        async move {
            debug!("ui task started");
            while let Ok(event) = ui_event_rx.recv().await {
                trace!(?event, "ui event received");
                match event {
                    UiEvent::WindowClosed => {
                        ui.borrow_mut().report_window_closed();
                        break;
                    }
                    UiEvent::Shutdown => {
                        ui.borrow_mut().report_window_closed();
                        break;
                    }
                    #[cfg(target_os = "macos")]
                    UiEvent::ShowPreferences => {
                        ui.borrow().show_preferences();
                    }
                    UiEvent::WindowFullscreenChanged(fullscreen) => {
                        ui.borrow_mut().handle_fullscreen_changed(fullscreen);
                    }
                    UiEvent::AppActivated(_hg) => {
                        if let Err(error) = ui.borrow_mut().activate() {
                            warn!(%error, "failed to activate UI");
                        }
                    }
                    UiEvent::WindowResized(size) => {
                        ui.borrow_mut().report_resize(size);
                    }
                    UiEvent::SetWindowMode(mode) => {
                        if let Err(error) = ui.borrow_mut().set_window_mode(mode) {
                            warn!(%error, "failed to set window mode");
                        }
                    }
                    UiEvent::MonitorsChanged => {
                        if let Err(error) = ui.borrow_mut().handle_monitors_changed() {
                            warn!(%error, "failed to handle monitor change");
                        }
                    }
                    UiEvent::Refresh => {
                        if let Err(error) = ui.borrow().request_display_config() {
                            warn!(%error, "failed to request display refresh");
                        }
                    }
                    UiEvent::SetShortcutPassthrough(enabled) => {
                        ui.borrow_mut().set_shortcut_passthrough(enabled);
                    }
                    UiEvent::ShortcutPassthroughStatus(active) => {
                        ui.borrow_mut().update_shortcut_passthrough_status(active);
                    }
                    UiEvent::UserConfirmedClose => break,
                }
            }
            debug!("ui task exiting");
            if let Some(app) = app_weak.upgrade() {
                app.quit();
            }
        }
    ))
}

fn connect_activate(app: &gtk::Application, ui_event_tx: Sender<UiEvent>) {
    app.connect_activate(move |app| {
        debug!("gtk application activated");
        ui_event_tx.send_or_warn(UiEvent::AppActivated(app.hold()));
    });
}

pub fn run(
    app: gtk::Application,
    ui: Ui,
    ui_event_tx: Sender<UiEvent>,
    ui_event_rx: Receiver<UiEvent>,
    conn: Arc<Connection>,
) {
    debug!("event loop starting");
    let ui = Rc::new(RefCell::new(ui));

    let (control_tx, control_rx) = unbounded::<ServerEvent>();
    let (draw_tx, draw_rx) = unbounded::<Draw>();
    trace!("server event channels created");

    let cancel_token = CancellationToken::new();
    let server_event_thread =
        spawn_server_event_thread(conn, cancel_token.clone(), control_tx, draw_tx);
    let control_task = spawn_control_task(ui.clone(), ui_event_tx.clone(), control_rx);
    let draw_task = spawn_draw_task(ui.clone(), draw_rx);
    let ui_task = spawn_ui_task(ui, ui_event_rx, app.downgrade());

    connect_activate(&app, ui_event_tx);
    app.run_with_args(&[""; 0]);

    // Ensure all UI tasks drop their Rc<Ui> before shutdown tries to unwrap the connection.
    control_task.abort();
    draw_task.abort();
    ui_task.abort();

    debug!("cancelling server event thread");
    cancel_token.cancel();
    if let Err(error) = server_event_thread.join() {
        error!(?error, "event thread panicked");
    }
    debug!("event loop shutdown complete");
}
