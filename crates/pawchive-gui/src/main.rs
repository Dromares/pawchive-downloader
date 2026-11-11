//! Desktop entry point. Android enters through `android_main` in the library (the loader needs a
//! cdylib); this thin bin keeps the desktop binary name and the packager metadata working.
fn main() {
    pawchive_gui::main();
}
