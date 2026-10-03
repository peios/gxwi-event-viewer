//! Event Viewer: what eventd has recorded on this machine, the events of
//! its kernel and programs, its metrics, and the logs of its services,
//! newest first and live. It opens on the events. With `--logs ORIGIN` it
//! opens instead on what one service, or whatever else logs as ORIGIN, has
//! written, which is what Services Manager's Logs button asks for; with
//! `--metrics`, on the person's dashboards of metrics.
//!
//! It asks eventd, on its query socket, as whoever is looking: what it
//! shows is what eventd lets them read, and it says what eventd keeps from
//! them. On a terminal, evctl does the same.

use std::sync::Arc;

use libgxwi::App;

mod chart;
mod dashboards;
mod metrics;
mod query;
mod viewer;
mod words;

use query::Kind;
use viewer::Viewer;

// What this program looks like, to whatever lists it. The icon itself is
// `gxwi-event-viewer.svg` at the repo root, installed as the base theme's.
libgxwi::icon!(b"dev.peios.gxwi-event-viewer");

fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let (kind, origin, metrics) = match arguments.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] | ["--events"] => (Kind::Events, None, false),
        ["--logs", origin] => (Kind::Logs, Some(origin.to_string()), false),
        ["--metrics"] => (Kind::Events, None, true),
        _ => {
            eprintln!("gxwi-event-viewer: usage: gxwi-event-viewer [--events | --metrics | --logs ORIGIN]");
            std::process::exit(64);
        }
    };
    let mut app = match App::connect() {
        Ok(app) => app,
        Err(e) => {
            eprintln!("gxwi-event-viewer: no desktop to open on: {e}");
            eprintln!("gxwi-event-viewer: on a terminal, evctl does what this does");
            std::process::exit(1);
        }
    };
    app.stylesheet("/gxwi-event-viewer.css", include_str!("gxwi-event-viewer.css"));
    let mut opened = query::Filter::new(kind);
    opened.origin = origin.clone().unwrap_or_default();
    let window = app.live(&viewer::title(&opened), Viewer::new(kind));
    let aside = Arc::downgrade(&window);
    window.update(|viewer, fields| {
        viewer.window = aside;
        if metrics {
            viewer.fill_metrics(fields);
        } else {
            viewer.fill(fields, origin.as_deref());
        }
    });
    if let Err(e) = app.run() {
        eprintln!("gxwi-event-viewer: {e}");
        std::process::exit(1);
    }
}
