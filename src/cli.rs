use clap::{Args, Parser, Subcommand};

/// Mask mode for `screenshot` / `zoom`: paint everything except the kept
/// windows with a solid fill. Without either flag the capture is unmasked.
#[derive(Debug, Clone, Default, Args)]
pub struct MaskArgs {
    /// Keep this window (an id from `windows`, decimal or 0x-hex); everything
    /// else is masked. Repeatable.
    #[arg(long = "keep-window", value_name = "ID", value_parser = parse_window_arg, conflicts_with = "mask_all")]
    pub keep_windows: Vec<u32>,
    /// Mask mode with an empty allowlist: every pixel is masked.
    #[arg(long)]
    pub mask_all: bool,
    /// Mask mode only: treat this see-through overlay of the calling app as
    /// absent (it hides nothing below it). Honoured only for a viewable
    /// override-redirect window whose _NET_WM_PID is this bridge's parent
    /// process and that is not kept; otherwise it is masked like any other
    /// window and reported in `mask.ignored_refused` / `ignored_missing`.
    /// Repeatable; does nothing without --keep-window / --mask-all.
    #[arg(long = "ignore-window", value_name = "ID", value_parser = parse_window_arg)]
    pub ignore_windows: Vec<u32>,
}

fn parse_window_arg(value: &str) -> Result<u32, String> {
    crate::wm::parse_window_id(value).map_err(|error| error.to_string())
}

#[derive(Debug, Parser)]
#[command(
    name = "x11-bridge",
    version,
    about = "X11 support bridge for Linux computer-use tooling."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Report connection info, X server details, and extension availability.
    Doctor,
    /// Enumerate screens (RandR monitors).
    Screens,
    /// Enumerate windows (EWMH).
    Windows,
    /// Report the current global cursor position in root coordinates.
    CursorPosition,
    /// Report the currently active (frontmost) app.
    FrontmostApp,
    /// Report the topmost app at a global point.
    AppUnderPoint {
        #[arg(long, allow_hyphen_values = true)]
        x: i32,
        #[arg(long, allow_hyphen_values = true)]
        y: i32,
    },
    /// Activate (raise + focus) a window by id.
    ActivateWindow {
        #[arg(long)]
        window: String,
    },
    /// Capture a full monitor and return an executor-style screenshot.
    Screenshot {
        #[arg(long)]
        display: Option<String>,
        #[command(flatten)]
        mask: MaskArgs,
    },
    /// Capture a logical region within a display and return an executor-style zoom image.
    Zoom {
        #[arg(long)]
        display: Option<String>,
        #[arg(long, allow_hyphen_values = true)]
        x: i32,
        #[arg(long, allow_hyphen_values = true)]
        y: i32,
        #[arg(long)]
        w: i32,
        #[arg(long)]
        h: i32,
        #[command(flatten)]
        mask: MaskArgs,
    },
    /// Move the pointer to a global point.
    PointerMove {
        #[arg(long, allow_hyphen_values = true)]
        x: i32,
        #[arg(long, allow_hyphen_values = true)]
        y: i32,
    },
    /// Click at a global point.
    PointerClick {
        #[arg(long = "modifier")]
        modifiers: Vec<String>,
        #[arg(long, allow_hyphen_values = true)]
        x: i32,
        #[arg(long, allow_hyphen_values = true)]
        y: i32,
        #[arg(long, default_value = "left")]
        button: String,
        #[arg(long, default_value_t = 1)]
        count: u32,
    },
    /// Scroll at a global point.
    PointerScroll {
        #[arg(long, allow_hyphen_values = true)]
        x: i32,
        #[arg(long, allow_hyphen_values = true)]
        y: i32,
        #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
        dx: f64,
        #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
        dy: f64,
    },
    /// Drag between two global points with the left button.
    PointerDrag {
        #[arg(long, allow_hyphen_values = true)]
        from_x: i32,
        #[arg(long, allow_hyphen_values = true)]
        from_y: i32,
        #[arg(long, allow_hyphen_values = true)]
        to_x: i32,
        #[arg(long, allow_hyphen_values = true)]
        to_y: i32,
    },
    /// Press and hold the left mouse button until explicitly released.
    LeftMouseDown,
    /// Release the left mouse button if it is currently held.
    LeftMouseUp,
    /// Send an executor-style key sequence such as `ctrl+c`.
    KeySequence {
        #[arg(long)]
        keys: String,
        #[arg(long)]
        repeat: Option<u32>,
    },
    /// Type text as individual key events.
    Type {
        #[arg(long)]
        text: String,
        #[arg(long, default_value_t = 12)]
        delay_ms: u64,
    },
    /// Hold one or more keys for a fixed duration in milliseconds.
    HoldKey {
        #[arg(long = "key", required = true)]
        keys: Vec<String>,
        #[arg(long)]
        duration_ms: u64,
    },
    /// Watch for the user's Escape (XInput2 raw keys, XTEST ignored) until
    /// stdin closes. JSON lines on stdout; see DESIGN.md.
    AbortWatch,
    /// Begin a session lock. On X11 this is a no-op that reports success.
    SessionStart {
        #[arg(long, default_value_t = false)]
        foreground: bool,
    },
    /// End a session lock. On X11 there is no daemon, so nothing is ended.
    SessionEnd,
    /// Forget the remembered screen-sharing grant. X11 has none: a no-op.
    ForgetSession,
    /// Turn accessibility on for a holder process (AT-SPI lease).
    AxEnable {
        #[arg(long)]
        holder_pid: u32,
    },
    /// Release the holder's accessibility lease; restores the previous state if last.
    AxRestore {
        #[arg(long)]
        holder_pid: u32,
    },
    /// List accessibility elements of a window (AT-SPI).
    AxFind {
        #[arg(long)]
        window: String,
        #[command(flatten)]
        ax: cu_atspi::cli::AxFindFlags,
    },
    /// Act on an element returned by `ax-find`. `set-text` reads stdin.
    AxAction {
        #[command(flatten)]
        ax: cu_atspi::cli::AxActionFlags,
    },
    /// List the accessibility features of this build as JSON. Needs no X
    /// server, D-Bus or session.
    AxFeatures,
}

#[cfg(test)]
mod tests {
    use super::*;
    use cu_atspi::ActionKind;

    fn parse(args: &[&str]) -> Result<Command, clap::Error> {
        let mut argv = vec!["x11-bridge"];
        argv.extend_from_slice(args);
        Cli::try_parse_from(argv).map(|c| c.command)
    }

    #[test]
    fn ax_find_old_flags_parse() {
        let Command::AxFind { window, ax } = parse(&[
            "ax-find",
            "--window",
            "0x1",
            "--role",
            "AXButton",
            "--title-contains",
            "Save",
            "--all",
            "--include-hidden",
            "--max-nodes",
            "10",
            "--max-depth",
            "4",
        ])
        .unwrap() else {
            panic!("not ax-find")
        };
        assert_eq!(window, "0x1");
        assert_eq!(ax.role.as_deref(), Some("AXButton"));
        assert_eq!(ax.title_contains.as_deref(), Some("Save"));
        assert!(ax.all && ax.include_hidden);
        assert_eq!((ax.max_nodes, ax.max_depth), (Some(10), Some(4)));
    }

    #[test]
    fn ax_find_new_flags_parse() {
        let Command::AxFind { ax, .. } = parse(&[
            "ax-find",
            "--window",
            "0x1",
            "--window-ref",
            "w",
            "--deadline-ms",
            "500",
            "--root-ref",
            "r",
            "--scope",
            "s",
        ])
        .unwrap() else {
            panic!("not ax-find")
        };
        assert_eq!(ax.deadline_ms, Some(500));
        assert_eq!(ax.root_ref.as_deref(), Some("r"));
        assert_eq!(ax.scope.as_deref(), Some("s"));
        let Command::AxFind { ax, .. } =
            parse(&["ax-find", "--window", "0x1", "--skeleton"]).unwrap()
        else {
            panic!("not ax-find")
        };
        assert!(ax.skeleton);
    }

    #[test]
    fn ax_find_root_ref_needs_scope() {
        let err = parse(&["ax-find", "--window", "0x1", "--root-ref", "r"]).unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn ax_find_skeleton_conflicts_with_filters() {
        let err = parse(&[
            "ax-find",
            "--window",
            "0x1",
            "--skeleton",
            "--role",
            "button",
        ])
        .unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn ax_action_kinds_parse() {
        for (name, kind) in [
            ("click", ActionKind::Click),
            ("set-text", ActionKind::SetText),
            ("ensure-checked", ActionKind::EnsureChecked),
            ("scroll-to", ActionKind::ScrollTo),
            ("set-value", ActionKind::SetValue),
        ] {
            let Command::AxAction { ax } =
                parse(&["ax-action", "--scope", "s", "--ref", "r", "--action", name]).unwrap()
            else {
                panic!("not ax-action")
            };
            assert_eq!(ax.action, kind, "{name}");
        }
        assert!(
            parse(&[
                "ax-action",
                "--scope",
                "s",
                "--ref",
                "r",
                "--action",
                "bogus"
            ])
            .is_err()
        );
    }

    #[test]
    fn ax_features_takes_no_flags() {
        assert!(matches!(parse(&["ax-features"]), Ok(Command::AxFeatures)));
        assert!(parse(&["ax-features", "--all"]).is_err());
    }
}
