use clap::{Args, Parser, Subcommand, ValueEnum};

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

/// Action for `ax-action`. `set-text` reads the text from stdin, never argv.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AxActionKind {
    Click,
    Focus,
    SetText,
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
        #[arg(long)]
        role: Option<String>,
        #[arg(long)]
        title_contains: Option<String>,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        include_hidden: bool,
        #[arg(long)]
        max_nodes: Option<usize>,
        #[arg(long)]
        max_depth: Option<u32>,
    },
    /// Act on an element returned by `ax-find`. `set-text` reads stdin.
    AxAction {
        #[arg(long)]
        scope: String,
        #[arg(long = "ref")]
        element_ref: String,
        #[arg(long, value_enum)]
        action: AxActionKind,
    },
}
