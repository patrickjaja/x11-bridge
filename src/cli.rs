use clap::{Parser, Subcommand};

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
    /// Begin a session lock. On X11 this is a no-op that reports success.
    SessionStart {
        #[arg(long, default_value_t = false)]
        foreground: bool,
    },
    /// End a session lock. On X11 there is no daemon, so nothing is ended.
    SessionEnd,
    /// Forget the remembered screen-sharing grant. X11 has none: a no-op.
    ForgetSession,
}
