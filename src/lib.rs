pub mod cmd;
pub mod image;

#[cfg(test)]
pub mod tests {
    use log::LevelFilter;
    use once_cell::sync::Lazy;
    use std::{path::PathBuf, sync::Once};

    static INIT: Once = Once::new();

    pub static DATA_DIR: Lazy<PathBuf> = Lazy::new(|| "tests/resources".parse().unwrap());

    #[cfg(test)]
    #[ctor::ctor]
    fn init() {
        INIT.call_once(|| {
            let crate_name = module_path!()
                .split("::")
                .next()
                .expect("get module_path error");
            env_logger::builder()
                .is_test(true)
                .filter_level(LevelFilter::Info)
                .filter_module(crate_name, LevelFilter::Trace)
                .init();
        });
    }
}
