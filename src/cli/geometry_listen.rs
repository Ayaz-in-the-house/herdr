//! `herdr pane geometry-listen` — where every visible pane is, on stdout.
//!
//! ⛔ **A CLIENT, not a tap** — the same rule `audio_listen` states and for the
//! same reason: reading the server's state directly would be shorter and would
//! lose capability negotiation, which is the thing that decides whether this
//! build can answer at all.
//!
//! What it is for: the geometry message has no consumer yet, so nothing about
//! it is observable. This makes it observable. Anything that can draw into a
//! rectangle — a player, a webview, a browser on its own socket — can be
//! synchronised to a pane using exactly what this prints.
//!
//! stdout carries the report; every diagnostic line goes to stderr, so the
//! output stays parseable by whatever consumes it next.

use std::io::{self, Write};

use crate::client::{client_socket_path, connect_geometry_listen_stream};
use crate::protocol::{self, ServerMessage, MAX_FRAME_SIZE};

/// What the caller asked for.
pub(crate) struct GeometryListenArgs {
    /// The ruler the rectangles arrive in.
    pub(crate) cell_width_px: u32,
    /// The ruler the rectangles arrive in.
    pub(crate) cell_height_px: u32,
    /// Print one JSON object per publication instead of a human report.
    pub(crate) json: bool,
}

pub(crate) fn run(args: GeometryListenArgs) -> io::Result<i32> {
    let socket_path = client_socket_path();
    let mut stream =
        match connect_geometry_listen_stream(&socket_path, args.cell_width_px, args.cell_height_px)
        {
            Ok(stream) => stream,
            Err(message) => {
                eprintln!("herdr: {message}");
                return Ok(1);
            }
        };

    eprintln!(
        "listening for pane geometry · cell {}x{} px · split, resize or switch tabs to see it move",
        args.cell_width_px, args.cell_height_px
    );

    let mut out = io::stdout().lock();
    loop {
        let message = match protocol::read_message(&mut stream, MAX_FRAME_SIZE) {
            Ok(message) => message,
            Err(err) => {
                eprintln!("herdr: server disconnected: {err}");
                return Ok(1);
            }
        };

        let ServerMessage::PaneGeometry {
            generation,
            cell_width_px,
            cell_height_px,
            panes,
        } = message
        else {
            // Every capable client is offered every pane's media too. Nothing
            // here reads it, and ignoring it costs nothing: this connection
            // declared no sink, so the server opens no stream for it.
            continue;
        };

        let written = if args.json {
            writeln!(
                out,
                "{}",
                json_line(generation, cell_width_px, cell_height_px, &panes)
            )
        } else {
            write_report(&mut out, generation, cell_width_px, cell_height_px, &panes)
        };
        if written.is_err() {
            // The consumer closed the pipe. It decided; this is not an error.
            return Ok(0);
        }
        if out.flush().is_err() {
            return Ok(0);
        }
    }
}

fn write_report(
    out: &mut impl Write,
    generation: u64,
    cell_w: u32,
    cell_h: u32,
    panes: &[crate::protocol::PaneRect],
) -> io::Result<()> {
    writeln!(
        out,
        "gen={generation}  cell={cell_w}x{cell_h}  panes={}",
        panes.len()
    )?;
    for pane in panes {
        writeln!(
            out,
            "  {:<20} outer={:>5},{:>5} {:>5}x{:<5} inner={:>5},{:>5} {:>5}x{:<5}{}",
            pane.pane_id,
            pane.x,
            pane.y,
            pane.width,
            pane.height,
            pane.inner_x,
            pane.inner_y,
            pane.inner_width,
            pane.inner_height,
            if pane.focused { " focus" } else { "" }
        )?;
    }
    Ok(())
}

fn json_line(
    generation: u64,
    cell_w: u32,
    cell_h: u32,
    panes: &[crate::protocol::PaneRect],
) -> String {
    let body: Vec<String> = panes
        .iter()
        .map(|pane| {
            format!(
                r#"{{"pane_id":{},"x":{},"y":{},"width":{},"height":{},"inner_x":{},"inner_y":{},"inner_width":{},"inner_height":{},"focused":{}}}"#,
                json_string(&pane.pane_id),
                pane.x,
                pane.y,
                pane.width,
                pane.height,
                pane.inner_x,
                pane.inner_y,
                pane.inner_width,
                pane.inner_height,
                pane.focused
            )
        })
        .collect();
    format!(
        r#"{{"generation":{generation},"cell_width_px":{cell_w},"cell_height_px":{cell_h},"panes":[{}]}}"#,
        body.join(",")
    )
}

/// Escape just enough for a pane id, which is generated and never arbitrary.
fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
