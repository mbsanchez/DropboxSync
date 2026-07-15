//! DBSYNC-62: entry point for the out-of-process CloudFilesContextMenus COM
//! ExeServer. Launched by the shell (with `-Embedding`) when a branded verb is
//! activated. See `exeserver::run_cloudfiles_exe_server`.

#![windows_subsystem = "windows"]

fn main() {
    #[cfg(windows)]
    {
        let _ = dropbox_sync_shell_menu::exeserver::run_cloudfiles_exe_server();
    }
}
