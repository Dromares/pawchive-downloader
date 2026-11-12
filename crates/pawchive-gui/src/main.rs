//! Desktop entry point. Android enters through `android_main` in the library (the loader needs a
//! cdylib); this thin bin keeps the desktop binary name and the packager metadata working.
//! Returning the `eframe::Result` makes a failed start exit non-zero with the error printed,
//! instead of silently quitting.
fn main() -> eframe::Result {
    pawchive_gui::main()
}
