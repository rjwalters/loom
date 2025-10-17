// Safe logging macros that don't panic when stderr is unavailable
// (common issue in macOS app bundles where stderr may not be connected)

#[macro_export]
macro_rules! safe_eprintln {
    ($($arg:tt)*) => {
        {
            use std::io::Write;
            let _ = writeln!(std::io::stderr(), $($arg)*);
        }
    };
}
