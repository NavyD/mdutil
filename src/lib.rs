pub mod image;

#[cfg(test)]
mod tests {
    use std::sync::Once;

    use log::LevelFilter;
    static INIT: Once = Once::new();

    #[cfg(test)]
    #[ctor::ctor]
    fn init() {
        INIT.call_once(|| {
            let crate_name = module_path!()
                .split("::")
                .collect::<Vec<_>>()
                .first()
                .cloned()
                .expect("get module_path error");
            env_logger::builder()
                .is_test(true)
                .filter_level(LevelFilter::Info)
                .filter_module(crate_name, LevelFilter::Trace)
                .init();
        });
    }
}
