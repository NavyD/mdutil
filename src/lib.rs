use once_cell::sync::Lazy;

pub mod cmd;
pub mod image;

pub static CRATE_NAME: Lazy<String> = Lazy::new(|| {
    module_path!()
        .split("::")
        .next()
        .map(ToString::to_string)
        .expect("get module_path error")
});

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
        use crate::CRATE_NAME;

        INIT.call_once(|| {
            env_logger::builder()
                .is_test(true)
                .filter_level(LevelFilter::Info)
                .filter_module(&CRATE_NAME, LevelFilter::Trace)
                .init();
        });
    }
}
