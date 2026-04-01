use std::{
    cell::{OnceCell, UnsafeCell},
    collections::HashMap,
    sync::{Arc, Mutex},
};

use javy_plugin_api::{
    Config, import_namespace,
    javy::{
        Runtime, from_js_error,
        quickjs::{Function, Module, Object, Persistent, prelude::*, promise::MaybePromise},
    },
};

import_namespace!("smelter-plugin");

wit_bindgen::generate!({ world: "smelter-plugin", generate_all });

static STATE: OnceCell<State> = OnceCell::new();

#[derive(Default)]
struct State {
    timeouts: HashMap<usize, Persistent<Function<'static>>>,
}

fn setup_runtime(runtime: Runtime) -> Runtime {
    runtime.context().with(|ctx| {
        let console = Object::new(ctx.clone()).unwrap();
        console
            .set("log", Func::from(|msg: String| crate::console_log(&msg)))
            .unwrap();
        console
            .set("warn", Func::from(|msg: String| crate::console_warn(&msg)))
            .unwrap();
        console
            .set(
                "error",
                Func::from(|msg: String| crate::console_error(&msg)),
            )
            .unwrap();

        ctx.globals().set("console", console).unwrap();
    });

    runtime
}

struct Component;

impl Guest for Component {
    fn compile_src(src: Vec<u8>) -> Result<Vec<u8>, String> {
        let source = String::from_utf8(src).map_err(|e| e.to_string())?;
        let wrapped = format!(
            "try {{\n{source}\n}} catch(e) {{ console.error(e.message + '\\n' + (e.stack || '')); }}"
        );
        javy_plugin_api::compile_src(wrapped.as_bytes()).map_err(|e| e.to_string())
    }

    fn initialize_runtime() {
        let config = || {
            let mut c = Config::default();
            c.text_encoding(true).javy_stream_io(true);
            c
        };
        javy_plugin_api::initialize_runtime(config, setup_runtime).unwrap();
    }

    fn invoke(bytecode: Vec<u8>, function: Option<String>) {
        if let Err(err) = javy_plugin_api::invoke(&bytecode, function.as_deref()) {
            crate::console_error(&err.to_string());
        }
    }
}

export!(Component);
