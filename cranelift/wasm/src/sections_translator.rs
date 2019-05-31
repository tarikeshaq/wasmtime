//! Helper functions to gather information for each of the non-function sections of a
//! WebAssembly module.
//!
//! The code of theses helper function is straightforward since it is only about reading metadata
//! about linear memories, tables, globals, etc. and storing them for later use.
//!
//! The special case of the initialize expressions for table elements offsets or global variables
//! is handled, according to the semantics of WebAssembly, to only specific expressions that are
//! interpreted on the fly.
use crate::environ::{ModuleEnvironment, WasmResult};
use crate::translation_utils::{
    type_to_type, FuncIndex, Global, GlobalIndex, GlobalInit, Memory, MemoryIndex, SignatureIndex,
    Table, TableElementType, TableIndex,
};
use core::convert::TryFrom;
use cranelift_codegen::ir::{self, AbiParam, Signature};
use cranelift_entity::EntityRef;
use std::vec::Vec;
use wasmparser::{
    self, CodeSectionReader, Data, DataKind, DataSectionReader, Element, ElementKind,
    ElementSectionReader, Export, ExportSectionReader, ExternalKind, FuncType,
    FunctionSectionReader, GlobalSectionReader, GlobalType, ImportSectionEntryType,
    ImportSectionReader, MemorySectionReader, MemoryType, Operator, TableSectionReader,
    TypeSectionReader,
};

/// Parses the Type section of the wasm module.
pub fn parse_type_section(
    types: TypeSectionReader,
    environ: &mut ModuleEnvironment,
) -> WasmResult<()> {
    environ.reserve_signatures(types.get_count());

    for entry in types {
        match entry? {
            FuncType {
                form: wasmparser::Type::Func,
                ref params,
                ref returns,
            } => {
                let mut sig = Signature::new(environ.target_config().default_call_conv);
                sig.params.extend(params.iter().map(|ty| {
                    let cret_arg: ir::Type = type_to_type(*ty)
                        .expect("only numeric types are supported in function signatures");
                    AbiParam::new(cret_arg)
                }));
                sig.returns.extend(returns.iter().map(|ty| {
                    let cret_arg: ir::Type = type_to_type(*ty)
                        .expect("only numeric types are supported in function signatures");
                    AbiParam::new(cret_arg)
                }));
                environ.declare_signature(sig);
            }
            ref s => panic!("unsupported type: {:?}", s),
        }
    }
    Ok(())
}

/// Parses the Import section of the wasm module.
pub fn parse_import_section<'data>(
    imports: ImportSectionReader<'data>,
    environ: &mut ModuleEnvironment<'data>,
) -> WasmResult<()> {
    environ.reserve_imports(imports.get_count());

    for entry in imports {
        let import = entry?;
        let module_name = import.module;
        let field_name = import.field;

        match import.ty {
            ImportSectionEntryType::Function(sig) => {
                environ.declare_func_import(SignatureIndex::from_u32(sig), module_name, field_name);
            }
            ImportSectionEntryType::Memory(MemoryType {
                limits: ref memlimits,
                shared,
            }) => {
                environ.declare_memory_import(
                    Memory {
                        minimum: memlimits.initial,
                        maximum: memlimits.maximum,
                        shared,
                    },
                    module_name,
                    field_name,
                );
            }
            ImportSectionEntryType::Global(ref ty) => {
                environ.declare_global_import(
                    Global {
                        ty: type_to_type(ty.content_type).unwrap(),
                        mutability: ty.mutable,
                        initializer: GlobalInit::Import,
                    },
                    module_name,
                    field_name,
                );
            }
            ImportSectionEntryType::Table(ref tab) => {
                environ.declare_table_import(
                    Table {
                        ty: match type_to_type(tab.element_type) {
                            Ok(t) => TableElementType::Val(t),
                            Err(()) => TableElementType::Func,
                        },
                        minimum: tab.limits.initial,
                        maximum: tab.limits.maximum,
                    },
                    module_name,
                    field_name,
                );
            }
        }
    }

    environ.finish_imports();
    Ok(())
}

/// Parses the Function section of the wasm module.
pub fn parse_function_section(
    functions: FunctionSectionReader,
    environ: &mut ModuleEnvironment,
) -> WasmResult<()> {
    environ.reserve_func_types(functions.get_count());

    for entry in functions {
        let sigindex = entry?;
        environ.declare_func_type(SignatureIndex::from_u32(sigindex));
    }

    Ok(())
}

/// Parses the Table section of the wasm module.
pub fn parse_table_section(
    tables: TableSectionReader,
    environ: &mut ModuleEnvironment,
) -> WasmResult<()> {
    environ.reserve_tables(tables.get_count());

    for entry in tables {
        let table = entry?;
        environ.declare_table(Table {
            ty: match type_to_type(table.element_type) {
                Ok(t) => TableElementType::Val(t),
                Err(()) => TableElementType::Func,
            },
            minimum: table.limits.initial,
            maximum: table.limits.maximum,
        });
    }

    Ok(())
}

/// Parses the Memory section of the wasm module.
pub fn parse_memory_section(
    memories: MemorySectionReader,
    environ: &mut ModuleEnvironment,
) -> WasmResult<()> {
    environ.reserve_memories(memories.get_count());

    for entry in memories {
        let memory = entry?;
        environ.declare_memory(Memory {
            minimum: memory.limits.initial,
            maximum: memory.limits.maximum,
            shared: memory.shared,
        });
    }

    Ok(())
}

/// Parses the Global section of the wasm module.
pub fn parse_global_section(
    globals: GlobalSectionReader,
    environ: &mut ModuleEnvironment,
) -> WasmResult<()> {
    environ.reserve_globals(globals.get_count());

    for entry in globals {
        let wasmparser::Global {
            ty: GlobalType {
                content_type,
                mutable,
            },
            init_expr,
        } = entry?;
        let mut init_expr_reader = init_expr.get_binary_reader();
        let initializer = match init_expr_reader.read_operator()? {
            Operator::I32Const { value } => GlobalInit::I32Const(value),
            Operator::I64Const { value } => GlobalInit::I64Const(value),
            Operator::F32Const { value } => GlobalInit::F32Const(value.bits()),
            Operator::F64Const { value } => GlobalInit::F64Const(value.bits()),
            Operator::GetGlobal { global_index } => {
                GlobalInit::GetGlobal(GlobalIndex::from_u32(global_index))
            }
            ref s => panic!("unsupported init expr in global section: {:?}", s),
        };
        let global = Global {
            ty: type_to_type(content_type).unwrap(),
            mutability: mutable,
            initializer,
        };
        environ.declare_global(global);
    }

    Ok(())
}

/// Parses the Export section of the wasm module.
pub fn parse_export_section<'data>(
    exports: ExportSectionReader<'data>,
    environ: &mut ModuleEnvironment<'data>,
) -> WasmResult<()> {
    environ.reserve_exports(exports.get_count());

    for entry in exports {
        let Export {
            field,
            ref kind,
            index,
        } = entry?;

        // The input has already been validated, so we should be able to
        // assume valid UTF-8 and use `from_utf8_unchecked` if performance
        // becomes a concern here.
        let index = index as usize;
        match *kind {
            ExternalKind::Function => environ.declare_func_export(FuncIndex::new(index), field),
            ExternalKind::Table => environ.declare_table_export(TableIndex::new(index), field),
            ExternalKind::Memory => environ.declare_memory_export(MemoryIndex::new(index), field),
            ExternalKind::Global => environ.declare_global_export(GlobalIndex::new(index), field),
        }
    }

    environ.finish_exports();
    Ok(())
}

/// Parses the Start section of the wasm module.
pub fn parse_start_section(index: u32, environ: &mut ModuleEnvironment) -> WasmResult<()> {
    environ.declare_start_func(FuncIndex::from_u32(index));
    Ok(())
}

/// Parses the Element section of the wasm module.
pub fn parse_element_section<'data>(
    elements: ElementSectionReader<'data>,
    environ: &mut ModuleEnvironment,
) -> WasmResult<()> {
    environ.reserve_table_elements(elements.get_count());

    for entry in elements {
        let Element { kind, items } = entry?;
        if let ElementKind::Active {
            table_index,
            init_expr,
        } = kind
        {
            let mut init_expr_reader = init_expr.get_binary_reader();
            let (base, offset) = match init_expr_reader.read_operator()? {
                Operator::I32Const { value } => (None, value as u32 as usize),
                Operator::GetGlobal { global_index } => {
                    (Some(GlobalIndex::from_u32(global_index)), 0)
                }
                ref s => panic!("unsupported init expr in element section: {:?}", s),
            };
            let items_reader = items.get_items_reader()?;
            let mut elems = Vec::with_capacity(usize::try_from(items_reader.get_count()).unwrap());
            for item in items_reader {
                let x = item?;
                elems.push(FuncIndex::from_u32(x));
            }
            environ.declare_table_elements(
                TableIndex::from_u32(table_index),
                base,
                offset,
                elems.into_boxed_slice(),
            )
        } else {
            panic!("unsupported passive elements section");
        }
    }
    Ok(())
}

/// Parses the Code section of the wasm module.
pub fn parse_code_section<'data>(
    code: CodeSectionReader<'data>,
    environ: &mut ModuleEnvironment<'data>,
) -> WasmResult<()> {
    for body in code {
        let mut reader = body?.get_binary_reader();
        let size = reader.bytes_remaining();
        let offset = reader.original_position();
        environ.define_function_body(reader.read_bytes(size)?, offset)?;
    }
    Ok(())
}

/// Parses the Data section of the wasm module.
pub fn parse_data_section<'data>(
    data: DataSectionReader<'data>,
    environ: &mut ModuleEnvironment<'data>,
) -> WasmResult<()> {
    environ.reserve_data_initializers(data.get_count());

    for entry in data {
        let Data { kind, data } = entry?;
        if let DataKind::Active {
            memory_index,
            init_expr,
        } = kind
        {
            let mut init_expr_reader = init_expr.get_binary_reader();
            let (base, offset) = match init_expr_reader.read_operator()? {
                Operator::I32Const { value } => (None, value as u32 as usize),
                Operator::GetGlobal { global_index } => {
                    (Some(GlobalIndex::from_u32(global_index)), 0)
                }
                ref s => panic!("unsupported init expr in data section: {:?}", s),
            };
            environ.declare_data_initialization(
                MemoryIndex::from_u32(memory_index),
                base,
                offset,
                data,
            );
        } else {
            panic!("unsupported passive data section");
        }
    }

    Ok(())
}
