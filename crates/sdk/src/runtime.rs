use std::{
    fs::{self, remove_dir_all, File},
    io::{stderr, stdin, BufRead, BufReader, Write},
    path::Path,
    sync::Arc,
};

use crate::{
    prelude::*,
    shared_config::{ClientFormat, SharedConfig},
};

use dmmf::from_precomputed_parts;
use query_core::schema;

use crate::{
    args::GenerateArgs, dmmf::EngineDMMF, jsonrpc, utils::rustfmt, GenerateFn, GeneratorError,
};

pub struct GeneratorMetadata {
    generate_fn: GenerateFn,
    name: &'static str,
    default_output: &'static str,
}

impl GeneratorMetadata {
    pub fn new(generate_fn: GenerateFn, name: &'static str, default_output: &'static str) -> Self {
        Self {
            generate_fn,
            name,
            default_output,
        }
    }

    pub fn run(self) {
        loop {
            let mut content = String::new();
            BufReader::new(stdin())
                .read_line(&mut content)
                .expect("Failed to read engine output");

            let input: jsonrpc::Request =
                serde_json::from_str(&content).expect("Failed to marshal jsonrpc input");

            let data = match input.method.as_str() {
                "getManifest" => jsonrpc::ResponseData::Result(
                    serde_json::to_value(jsonrpc::ManifestResponse {
                        manifest: jsonrpc::Manifest {
                            default_output: self.default_output.to_string(),
                            pretty_name: self.name.to_string(),
                            ..Default::default()
                        },
                    })
                    .expect("Failed to convert manifest to json"), // literally will never fail
                ),
                "generate" => {
                    let params_str = input.params.to_string();

                    let deserializer = &mut serde_json::Deserializer::from_str(&params_str);

                    let dmmf = serde_path_to_error::deserialize(deserializer)
                        .expect("Failed to deserialize DMMF from Prisma engines");

                    match self.generate(dmmf) {
                        Ok(_) => jsonrpc::ResponseData::Result(serde_json::Value::Null),
                        Err(e) => jsonrpc::ResponseData::Error {
                            code: 0,
                            message: e.to_string(),
                        },
                    }
                }
                method => jsonrpc::ResponseData::Error {
                    code: 0,
                    message: format!("{} cannot handle method {}", self.name, method),
                },
            };

            let response = jsonrpc::Response {
                jsonrpc: "2.0".to_string(),
                id: input.id,
                data,
            };

            let mut bytes =
                serde_json::to_vec(&response).expect("Failed to marshal json data for reply");

            bytes.push(b'\n');

            stderr()
                .by_ref()
                .write(bytes.as_ref())
                .expect("Failed to write output to stderr for Prisma engines");

            if input.method.as_str() == "generate" {
                break;
            }
        }
    }

    fn generate(&self, engine_dmmf: EngineDMMF) -> Result<(), GeneratorError> {
        let schema = Arc::new(
            psl::parse_schema(engine_dmmf.datamodel.as_str())
                .expect("Datamodel is invalid after being verified by CLI?!"),
        );
        let query_schema = Arc::new(schema::build(schema.clone(), true));
        let dmmf = from_precomputed_parts(&query_schema);

        let output_str = engine_dmmf.generator.output.get_value();
        let root_output_path = Path::new(&output_str);

        let config = engine_dmmf.generator.config.clone();

        let shared_config: SharedConfig =
            serde_json::from_value(serde_json::Value::Object(config.clone())).unwrap();

        match shared_config.client_format {
            ClientFormat::Folder if root_output_path.extension().is_some() => {
                panic!("The output path must be a directory when using the folder format.")
            }
            ClientFormat::File if root_output_path.extension().is_none() => {
                panic!("The output path must be a file when using the file format.")
            }
            _ => {}
        }

        let root_module =
            (self.generate_fn)(GenerateArgs::new(&schema, &dmmf, engine_dmmf), config)?;

        remove_dir_all(root_output_path).ok();

        let header = format!("// File generated by {}. DO NOT EDIT\n\n", self.name);

        match shared_config.client_format {
            ClientFormat::Folder => {
                write_module_to_file(&root_module, root_output_path, &header);
            }
            ClientFormat::File => write_to_file(&root_module.flatten(), root_output_path, &header),
        }

        rustfmt(&root_module.get_all_paths(root_output_path));

        Ok(())
    }
}

fn write_module_to_file(module: &Module, parent_path: &Path, header: &str) {
    if !module.submodules.is_empty() {
        for child in &module.submodules {
            write_module_to_file(
                child,
                &parent_path.join(child.name.to_case(Case::Snake, true)),
                header,
            );
        }

        let contents = &module.contents;
        let submodule_decls = module.submodules.iter().map(|sm| {
            let name = snake_ident(&sm.name);
            quote!(pub mod #name;)
        });

        write_to_file(
            &quote! {
                #(#submodule_decls)*

                #contents
            },
            &parent_path.join("mod.rs"),
            header,
        );
    } else {
        write_to_file(&module.contents, &parent_path.with_extension("rs"), header);
    }
}

fn write_to_file(contents: &TokenStream, path: &Path, header: &str) {
    let mut file = create_generated_file(path).unwrap();

    file.write((header.to_string() + &contents.to_string()).as_bytes())
        .unwrap();
}

fn create_generated_file(path: &Path) -> Result<File, GeneratorError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(GeneratorError::FileCreate)?;
    }

    File::create(path).map_err(GeneratorError::FileCreate)
}
