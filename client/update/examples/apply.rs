//! Live test of the update swap on a real install (used by the macOS checks
//! workflow): `apply PROGRAM_OR_BUNDLE_PROGRAM DOWNLOADED_FILE`.
//! Prints how the install is seen, then applies the file as the agent would and
//! says whether the program file was replaced.

fn main() -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        let args: Vec<String> = std::env::args().collect();
        anyhow::ensure!(args.len() == 3, "usage: apply PROGRAM DOWNLOADED_FILE");
        let program = std::path::Path::new(&args[1]);
        let install = nuvira_update::unix::this_install(program);
        println!("install: {install:?}, channel {:?}", install.channel());
        let replaced = nuvira_update::unix::Replaced::watch(program);
        let result = nuvira_update::unix::apply(&install, std::path::Path::new(&args[2]));
        println!("apply: {result:?}");
        println!("replaced: {}", replaced.happened());
        nuvira_update::unix::clean(&install);
        result
    }
    #[cfg(not(unix))]
    anyhow::bail!("Linux and macOS only")
}
