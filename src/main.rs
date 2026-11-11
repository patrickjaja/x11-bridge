mod abort;
mod ax;
mod capture;
mod cli;
mod conn;
mod doctor;
mod input;
mod mask;
#[cfg(test)]
mod mask_xvfb_it;
mod output;
mod wm;

use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::Parser;

use crate::cli::{Cli, Command};
use crate::conn::Conn;
use crate::input::pointer::Button;
use crate::output::print_json;

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("{error:#}");
            if error.downcast_ref::<ax::UsageError>().is_some() {
                return ExitCode::from(2);
            }
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode> {
    let cli = Cli::parse();

    match cli.command {
        // Session lifecycle is a no-op on X11: there is no portal session to
        // acquire (unlike Wayland/KWin, where a ScreenCast+RemoteDesktop portal
        // session must stay alive for the tool-use lock). XTEST input and
        // GetImage capture work statelessly per-invocation, so we simply report
        // success and let the JS session bookkeeping proceed. See DESIGN.md.
        Command::SessionStart { foreground } => {
            let _ = foreground;
            print_json(&serde_json::json!({ "ok": true, "session": "noop" }))?;
        }
        Command::SessionEnd => {
            // Literal strings: serde_json's map sorts keys, and the contract
            // documents this exact byte shape.
            println!(r#"{{"ok":true,"ended":false,"daemon":"none"}}"#);
        }
        Command::ForgetSession => {
            println!(
                r#"{{"ok":true,"ended":false,"daemon":"none","token_removed":false,"token_path":null}}"#
            );
        }

        Command::Doctor => {
            print_json(&doctor::contract_report())?;
        }
        // Long-running: the shared crate owns stdout, stdin and the exit code.
        Command::AbortWatch => return Ok(cu_abort_watch::run(abort::Xi2Backend::new())),
        Command::AxEnable { holder_pid } => {
            print_json(&ax::enable(holder_pid)?)?;
        }
        Command::AxRestore { holder_pid } => {
            print_json(&ax::restore(holder_pid)?)?;
        }
        Command::AxFind {
            window,
            role,
            title_contains,
            all,
            include_hidden,
            max_nodes,
            max_depth,
        } => {
            let conn = Conn::connect()?;
            let windows = wm::windows(&conn)?;
            let args = ax::FindArgs {
                window,
                role,
                title_contains,
                all,
                include_hidden,
                max_nodes,
                max_depth,
            };
            print_json(&ax::find(&conn, &windows, &args)?)?;
        }
        Command::AxAction {
            scope,
            element_ref,
            action,
        } => {
            print_json(&ax::action(
                &scope,
                &element_ref,
                action,
                std::io::stdin().lock(),
            )?)?;
        }
        Command::Screens => {
            let conn = Conn::connect()?;
            let monitors = conn.monitors()?;
            // Single-head fallback: some setups report no RandR primary, which
            // would leave `is_primary`/`is_active` false on every entry. When
            // there is exactly one monitor it is unambiguously the primary and
            // active one, so force both true; the JS otherwise falls back to
            // displays[0] but expects a truthy primary for display selection.
            let single_head = monitors.len() == 1;
            let screens: Vec<output::Screen> = monitors
                .into_iter()
                .map(|monitor| {
                    let is_primary = monitor.primary || single_head;
                    output::Screen {
                        geometry: output::Rect {
                            x: monitor.x,
                            y: monitor.y,
                            width: monitor.width,
                            height: monitor.height,
                        },
                        is_active: is_primary,
                        is_primary,
                        scale: Some(1.0),
                        refresh_millihz: None,
                        name: monitor.name.clone(),
                        id: monitor.name,
                    }
                })
                .collect();
            print_json(&screens)?;
        }
        Command::Windows => {
            let conn = Conn::connect()?;
            print_json(&wm::windows(&conn)?)?;
        }
        Command::CursorPosition => {
            let conn = Conn::connect()?;
            print_json(&pointer_cursor_position(&conn)?)?;
        }
        Command::FrontmostApp => {
            let conn = Conn::connect()?;
            print_json(&wm::frontmost_app(&conn)?)?;
        }
        Command::AppUnderPoint { x, y } => {
            let conn = Conn::connect()?;
            print_json(&wm::app_under_point(&conn, x, y)?)?;
        }
        Command::ActivateWindow { window } => {
            let conn = Conn::connect()?;
            print_json(&wm::activate_window(&conn, &window)?)?;
        }
        Command::Screenshot {
            display,
            mask: mask_args,
        } => {
            let request = mask::MaskRequest::from_cli(
                mask_args.keep_windows,
                mask_args.mask_all,
                mask_args.ignore_windows,
            );
            let conn = Conn::connect()?;
            print_json(&capture::screenshot(
                &conn,
                display.as_deref(),
                request.as_ref(),
            )?)?;
        }
        Command::Zoom {
            display,
            x,
            y,
            w,
            h,
            mask: mask_args,
        } => {
            let request = mask::MaskRequest::from_cli(
                mask_args.keep_windows,
                mask_args.mask_all,
                mask_args.ignore_windows,
            );
            let conn = Conn::connect()?;
            print_json(&capture::zoom(
                &conn,
                display.as_deref(),
                (x, y, w, h),
                request.as_ref(),
            )?)?;
        }
        Command::PointerMove { x, y } => {
            let conn = Conn::connect()?;
            print_json(&input::pointer::move_pointer(&conn, x, y)?)?;
        }
        Command::PointerClick {
            modifiers,
            x,
            y,
            button,
            count,
        } => {
            let conn = Conn::connect()?;
            let button = Button::parse(&button)?;
            print_json(&input::pointer::click(
                &conn, x, y, button, count, &modifiers,
            )?)?;
        }
        Command::PointerScroll { x, y, dx, dy } => {
            let conn = Conn::connect()?;
            print_json(&input::pointer::scroll(&conn, x, y, dx, dy)?)?;
        }
        Command::PointerDrag {
            from_x,
            from_y,
            to_x,
            to_y,
        } => {
            let conn = Conn::connect()?;
            print_json(&input::pointer::drag(&conn, from_x, from_y, to_x, to_y)?)?;
        }
        Command::LeftMouseDown => {
            let conn = Conn::connect()?;
            print_json(&input::pointer::left_mouse_down(&conn)?)?;
        }
        Command::LeftMouseUp => {
            let conn = Conn::connect()?;
            print_json(&input::pointer::left_mouse_up(&conn)?)?;
        }
        Command::KeySequence { keys, repeat } => {
            let conn = Conn::connect()?;
            print_json(&input::keyboard::key_sequence(&conn, &keys, repeat)?)?;
        }
        Command::Type { text, delay_ms } => {
            let conn = Conn::connect()?;
            print_json(&input::keyboard::type_text(&conn, &text, delay_ms)?)?;
        }
        Command::HoldKey { keys, duration_ms } => {
            let conn = Conn::connect()?;
            print_json(&input::keyboard::hold_key(&conn, &keys, duration_ms)?)?;
        }
    }

    Ok(ExitCode::SUCCESS)
}

/// Report the current global cursor position via xproto.
///
/// `query_pointer` on the root window yields `root_x`/`root_y`, the pointer's
/// position in X11 root (global) pixel coordinates — the same space the JS
/// contract expects.
fn pointer_cursor_position(conn: &Conn) -> Result<output::CursorPosition> {
    use x11rb::protocol::xproto::ConnectionExt;

    let reply = conn
        .conn
        .query_pointer(conn.root)
        .context("failed to query the pointer position")?
        .reply()
        .context("failed to read the pointer position reply")?;

    Ok(output::CursorPosition {
        x: i32::from(reply.root_x),
        y: i32::from(reply.root_y),
    })
}
