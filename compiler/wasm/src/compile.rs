use fm::FileManager;
use gloo_utils::format::JsValueSerdeExt;
use js_sys::Array;
use nargo::artifacts::{
    contract::{PreprocessedContract, PreprocessedContractFunction},
    program::PreprocessedProgram,
};
use noirc_driver::{
    add_dep, compile_contract, compile_main, prepare_crate, prepare_dependency, CompileOptions,
    CompiledContract, CompiledProgram,
};
use noirc_frontend::{
    graph::{CrateGraph, CrateName},
    hir::Context,
};
use serde::Deserialize;
use std::{
    path::{Path, PathBuf},
    str::FromStr,
};
use wasm_bindgen::prelude::*;

use crate::errors::JsCompileError;

const BACKEND_IDENTIFIER: &str = "acvm-backend-barretenberg";

#[derive(Deserialize)]
struct Dependency {
    name: String,
    package_root: PathBuf,
}

#[wasm_bindgen(typescript_custom_section)]
const DEPENDENCY_TYPE: &'static str = r#"
export type Dependency = {
    name: string,
    package_root: string
}
"#;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(extends = Array, js_name = "DependencyArray", typescript_type = "Dependency[]")]
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub type DependencyArray;
}

#[wasm_bindgen]
pub fn compile(
    entry_point: String,
    contracts: Option<bool>,
    dependencies: Option<DependencyArray>,
) -> Result<JsValue, JsCompileError> {
    console_error_panic_hook::set_once();

    let root = Path::new("/");
    let fm = FileManager::new(root, Box::new(get_non_stdlib_asset));
    let graph = CrateGraph::default();
    let mut context = Context::new(fm, graph);

    let path = Path::new(&entry_point);
    let crate_id = prepare_crate(&mut context, path);

    let dependencies: Vec<Dependency> = dependencies
        .map(|array| {
            array
                .iter()
                .map(|dep| {
                    JsValueSerdeExt::into_serde(&dep)
                        .expect("Could not deserialize dependency argument")
                })
                .collect()
        })
        .unwrap_or_default();
    for dependency in dependencies {
        add_noir_lib(&mut context, dependency);
    }

    let compile_options = CompileOptions::default();

    // For now we default to plonk width = 3, though we can add it as a parameter
    let np_language = acvm::Language::PLONKCSat { width: 3 };
    #[allow(deprecated)]
    let is_opcode_supported = acvm::pwg::default_is_opcode_supported(np_language);

    if contracts.unwrap_or_default() {
        let compiled_contract = compile_contract(&mut context, crate_id, &compile_options)
            .map_err(|_| JsCompileError::new("Failed to compile contract".to_string()))?
            .0;

        let optimized_contract =
            nargo::ops::optimize_contract(compiled_contract, np_language, &is_opcode_supported)
                .expect("Contract optimization failed");

        let preprocessed_contract = preprocess_contract(optimized_contract);

        Ok(<JsValue as JsValueSerdeExt>::from_serde(&preprocessed_contract).unwrap())
    } else {
        let compiled_program = compile_main(&mut context, crate_id, &compile_options, None, true)
            .map_err(|_| JsCompileError::new("Failed to compile program".to_string()))?
            .0;

        let optimized_program =
            nargo::ops::optimize_program(compiled_program, np_language, &is_opcode_supported)
                .expect("Program optimization failed");

        let preprocessed_program = preprocess_program(optimized_program);

        Ok(<JsValue as JsValueSerdeExt>::from_serde(&preprocessed_program).unwrap())
    }
}

fn add_noir_lib(context: &mut Context, library: Dependency) {
    let path_to_lib = library.package_root.join("src/lib.nr");
    let library_crate_id = prepare_dependency(context, &path_to_lib);

    let library_name = CrateName::from_str(&library.name).unwrap();
    add_dep(context, *context.root_crate_id(), library_crate_id, library_name.clone());

    // TODO: Remove this code that attaches every crate to every other crate as a dependency
    let root_crate_id = context.root_crate_id();
    let stdlib_crate_id = context.stdlib_crate_id();
    let other_crate_ids: Vec<_> = context
        .crate_graph
        .iter_keys()
        .filter(|crate_id| {
            // We don't want to attach this crate to itself or stdlib, nor re-attach it to the root crate
            crate_id != &library_crate_id
                && crate_id != root_crate_id
                && crate_id != stdlib_crate_id
        })
        .collect();

    for crate_id in other_crate_ids {
        context
            .crate_graph
            .add_dep(crate_id, library_name.clone(), library_crate_id)
            .unwrap_or_else(|_| panic!("ICE: Cyclic error triggered by {library_name} library"));
    }
}

fn preprocess_program(program: CompiledProgram) -> PreprocessedProgram {
    PreprocessedProgram {
        hash: program.hash,
        backend: String::from(BACKEND_IDENTIFIER),
        abi: program.abi,
        bytecode: program.circuit,
    }
}

fn preprocess_contract(contract: CompiledContract) -> PreprocessedContract {
    let preprocessed_functions = contract
        .functions
        .into_iter()
        .map(|func| PreprocessedContractFunction {
            name: func.name,
            function_type: func.function_type,
            is_internal: func.is_internal,
            abi: func.abi,
            bytecode: func.bytecode,
        })
        .collect();

    PreprocessedContract {
        name: contract.name,
        backend: String::from(BACKEND_IDENTIFIER),
        functions: preprocessed_functions,
        events: contract.events,
    }
}

cfg_if::cfg_if! {
    if #[cfg(target_os = "wasi")] {
        fn get_non_stdlib_asset(path_to_file: &Path) -> std::io::Result<String> {
            std::fs::read_to_string(path_to_file)
        }
    } else {
        use std::io::{Error, ErrorKind};

        #[wasm_bindgen(module = "@noir-lang/source-resolver")]
        extern "C" {
            #[wasm_bindgen(catch)]
            fn read_file(path: &str) -> Result<String, JsValue>;
        }

        fn get_non_stdlib_asset(path_to_file: &Path) -> std::io::Result<String> {
            let path_str = path_to_file.to_str().unwrap();
            match read_file(path_str) {
                Ok(buffer) => Ok(buffer),
                Err(_) => Err(Error::new(ErrorKind::Other, "could not read file using wasm")),
            }
        }
    }
}
