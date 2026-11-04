// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // The self-update's watchdog is this binary re-run from the old bundle
    // (ADR-0034): it does one job and must not start the app.
    if faber_menubar_lib::maybe_run_watchdog() {
        return;
    }
    // release.yml asks the built bundle which verification policy it enforces.
    if faber_menubar_lib::maybe_print_policy() {
        return;
    }
    faber_menubar_lib::run()
}
