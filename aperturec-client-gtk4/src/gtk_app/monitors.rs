#[cfg(target_os = "windows")]
use super::DEFAULT_RESOLUTION;
use super::ensure_gtk_init;
use anyhow::{Result, bail};
use aperturec_client::state::LockState;
use aperturec_graphics::{
    display::{Display as GraphicsDisplay, DisplayConfiguration},
    euclid_collections::{EuclidMap, EuclidSet},
    prelude::*,
};
use gtk4::{gdk, prelude::*};
#[cfg(target_os = "windows")]
use tracing::warn;
use tracing::{debug, trace};

#[derive(Debug, Clone, Default)]
pub struct Monitors {
    map: EuclidMap<(Rect, gdk::Monitor)>,
}

impl Monitors {
    #[allow(unused)]
    pub fn total_areas(&self) -> impl Iterator<Item = Rect> {
        self.map.keys()
    }

    pub fn usable_areas(&self) -> impl Iterator<Item = Rect> {
        self.map.values().map(|&(usable_area, _)| usable_area)
    }

    pub fn matches(&self, display_configuration: &DisplayConfiguration) -> bool {
        let result = display_configuration
            .display_decoder_infos
            .iter()
            .filter(|ddi| ddi.display.is_enabled)
            .map(|ddi| ddi.display.area)
            .collect::<EuclidSet>()
            == self.usable_areas().collect();
        trace!(matches = result, "monitor geometry matches display config");
        result
    }

    #[allow(unused)]
    pub fn is_multi(&self) -> bool {
        self.map.len() > 1
    }

    pub fn as_displays(&self) -> impl Iterator<Item = GraphicsDisplay> + '_ {
        self.map
            .iter()
            .map(|(_, &(usable_area, _))| GraphicsDisplay {
                area: usable_area,
                is_enabled: true,
            })
    }

    pub fn current() -> Result<Self> {
        ensure_gtk_init();
        debug!("querying current monitors");

        fn gdkrect2rect(geo: gdk::Rectangle) -> Rect {
            Rect::new(
                Point::new(geo.x() as _, geo.y() as _),
                Size::new(geo.width() as _, geo.height() as _),
            )
        }

        fn monitor_workarea(mon: &gdk::Monitor) -> Rect {
            #[cfg(target_os = "macos")]
            {
                gdkrect2rect(gdk4_macos::MacosMonitor::workarea(mon))
            }

            #[cfg(target_os = "windows")]
            {
                warn!("{:?} ignored on windows, defaulting to 800x600", mon);
                Rect::new(Point::new(0, 0), DEFAULT_RESOLUTION)
                // unimplemented!("unsure about how to get the monitors on windows")
                // gdkrect2rect(gdk4_win32::Win32Monitor::workarea(mon))
            }

            #[cfg(target_os = "linux")]
            {
                gdkrect2rect(mon.geometry())
            }
        }

        let Some(display) = gdk::Display::default() else {
            bail!("no default display");
        };

        let mut map = EuclidMap::new();
        let monitor_objs_list = display.monitors();
        debug!(
            count = monitor_objs_list.n_items(),
            "monitors reported by display"
        );

        if monitor_objs_list.n_items() == 0 {
            bail!("no monitors found for display {display:?}");
        }

        for mon_res in monitor_objs_list.iter::<gdk::Monitor>() {
            let mon = mon_res?;

            let total_area = gdkrect2rect(mon.geometry());
            let usable_area = monitor_workarea(&mon);
            trace!(?total_area, ?usable_area, "monitor geometry");
            if !total_area.contains_rect(&usable_area) {
                bail!("usable area exceeds total area of monitor");
            }

            let overlaps = map.insert(total_area, (usable_area, mon.clone()));
            if !overlaps.is_empty() {
                bail!("at least two monitors overlap: {total_area:?} and {overlaps:?}");
            }
        }

        if map.is_empty() {
            bail!("no monitors");
        }

        Ok(Monitors { map })
    }

    pub fn gdk_monitor_at_point(&self, point: Point) -> Option<gdk::Monitor> {
        self.map.get(point).map(|(_, (_, mon))| mon.clone())
    }
}

pub fn current_lock_state() -> Result<LockState> {
    ensure_gtk_init();
    trace!("querying lock state");

    let Some(display) = gdk::Display::default() else {
        bail!("no default display");
    };
    let Some(seat) = display.default_seat() else {
        bail!("no default seat");
    };
    let Some(keyboard) = seat.keyboard() else {
        bail!("no keyboard");
    };
    Ok(LockState {
        is_caps_locked: keyboard.is_caps_locked(),
        is_num_locked: keyboard.is_num_locked(),
        is_scroll_locked: keyboard.is_scroll_locked(),
    })
}
