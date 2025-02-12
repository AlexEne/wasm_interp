use anyhow::{anyhow, Result};
use std::cell::{Ref, RefCell, RefMut};
use std::collections::HashMap;
use std::convert::TryFrom;
use std::fs::File;
use std::io::BufReader;
use std::io::Read;
use std::rc::Rc;

use crate::core::{
    self, evaluate_constant_expression,
    stack_entry::StackEntry,
    store_access::{CellRefMutType, CellRefType, RefType},
    Callable, ConstantExpressionStore, ExpressionStore, FuncType, Global, Memory, Stack, Table,
};
use crate::parser::InstructionSource;
use crate::reader::{ModuleBuilder, ReaderUtil, ScopedReader, TypeReader};

#[derive(Debug)]
struct RawModuleMetadata {
    types: Vec<core::FuncType>,
}

#[derive(Debug)]
pub struct RawModule {
    metadata: RawModuleMetadata,
    typeidx: Vec<usize>,
    funcs: Vec<core::Func>,
    tables: Vec<core::TableType>,
    mems: Vec<core::MemType>,
    globals: Vec<core::GlobalDef>,
    elem: Vec<core::Element>,
    data: Vec<core::Data>,
    start: Option<usize>,
    imports: Vec<core::Import>,
    exports: Vec<core::Export>,
}

impl TypeReader for core::RawModule {
    fn read<T: Read>(reader: &mut T) -> Result<Self> {
        const HEADER_LENGTH: usize = 8;
        const EXPECTED_HEADER: [u8; 8] = [0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00];

        let mut header: [u8; HEADER_LENGTH] = [0; HEADER_LENGTH];

        // Read in the header
        reader.read_exact(&mut header)?;

        if header != EXPECTED_HEADER {
            Err(anyhow!("Invalid module header"))
        } else {
            let mut current_section_type: Option<core::SectionType> =
                Some(core::SectionType::TypeSection);
            let mut module_builder = ModuleBuilder::new();

            loop {
                if let Ok(section_type) = ModuleBuilder::read_next_section_header(reader) {
                    // Read the section length
                    let section_length = usize::try_from(reader.read_leb_u32()?).unwrap();
                    // And make a scoped reader for the section
                    let mut section_reader = ScopedReader::new(reader, section_length);

                    // Always skip custom sections wherever they appear
                    if section_type == core::SectionType::CustomSection {
                        // Read the section name
                        let section_name = section_reader.read_name()?;
                        let _section_body = section_reader.read_bytes_to_end()?;

                        println!("Skipping custom section \"{}\"", section_name);
                    } else {
                        while let Some(expected_section_type) = current_section_type {
                            if expected_section_type == section_type {
                                // This is the correct section type so we process it and move on
                                module_builder
                                    .process_section(section_type, &mut section_reader)?;

                                // And the next section type is the same as this one
                                current_section_type = Some(expected_section_type);
                                break;
                            } else {
                                // The section type doesn't match, so we move on to see if it
                                // is the next valid section
                                current_section_type =
                                    ModuleBuilder::get_next_section_type(expected_section_type);
                            }
                        }

                        if current_section_type == None {
                            assert!(false, "Sections are in unexpected order");
                            return Err(anyhow!("Invalid section order"));
                        }
                    }

                    if !section_reader.is_at_end() {
                        assert!(false, "Failed to read whole section");
                        return Err(anyhow!("Failed to read whole section"));
                    }
                } else {
                    // End of file, so we can break out of the loop
                    break;
                }
            }

            module_builder.make_module()
        }
    }
}

impl RawModule {
    pub fn new(
        types: Vec<core::FuncType>,
        typeidx: Vec<usize>,
        funcs: Vec<core::Func>,
        tables: Vec<core::TableType>,
        mems: Vec<core::MemType>,
        globals: Vec<core::GlobalDef>,
        elem: Vec<core::Element>,
        data: Vec<core::Data>,
        start: Option<usize>,
        imports: Vec<core::Import>,
        exports: Vec<core::Export>,
    ) -> Self {
        Self {
            metadata: RawModuleMetadata { types },
            typeidx,
            funcs,
            tables,
            mems,
            globals,
            elem,
            data,
            start,
            imports,
            exports,
        }
    }
}

#[derive(Debug)]
pub enum ExportValue {
    Function(Rc<RefCell<Callable>>),
    Table(Rc<RefCell<Table>>),
    Memory(Rc<RefCell<Memory>>),
    Global(Rc<RefCell<Global>>),
}

#[derive(Debug)]
pub struct Module {
    pub functions: Vec<Rc<RefCell<Callable>>>,
    pub tables: Vec<Rc<RefCell<Table>>>,
    pub memories: Vec<Rc<RefCell<Memory>>>,
    pub globals: Vec<Rc<RefCell<Global>>>,
    pub exports: HashMap<String, ExportValue>,
    func_types: Vec<FuncType>,
}

impl Module {
    pub fn new() -> Self {
        Self {
            functions: Vec::new(),
            tables: Vec::new(),
            memories: Vec::new(),
            globals: Vec::new(),
            exports: HashMap::new(),
            func_types: Vec::new(),
        }
    }

    pub fn load_module_from_path<R: core::Resolver>(
        file: &str,
        resolver: &R,
    ) -> anyhow::Result<Self> {
        let mut buf = BufReader::new(File::open(file)?);
        let raw_module = core::RawModule::read(&mut buf)?;
        let module = core::Module::resolve_raw_module(raw_module, resolver)?;
        Ok(module)
    }

    fn resolve_imports<Iter: Iterator<Item = core::Import>, Resolver: core::Resolver>(
        &mut self,
        imports: Iter,
        metadata: &RawModuleMetadata,
        resolver: &Resolver,
    ) -> Result<()> {
        for import in imports {
            match import.desc() {
                core::ImportDesc::TypeIdx(type_index) => {
                    if *type_index >= metadata.types.len() {
                        return Err(anyhow!(
                            "Function import {} from module {} has invalid type index",
                            import.mod_name(),
                            import.name()
                        ));
                    }

                    let resolved_function = resolver.resolve_function(
                        import.mod_name(),
                        import.name(),
                        &metadata.types[*type_index],
                    )?;
                    self.functions.push(resolved_function);
                }
                core::ImportDesc::TableType(table_type) => {
                    let resolved_table =
                        resolver.resolve_table(import.mod_name(), import.name(), table_type)?;
                    self.tables.push(resolved_table);
                }
                core::ImportDesc::MemType(mem_type) => {
                    let resolved_memory =
                        resolver.resolve_memory(import.mod_name(), import.name(), mem_type)?;
                    self.memories.push(resolved_memory);
                }
                core::ImportDesc::GlobalType(global_type) => {
                    let resolved_global =
                        resolver.resolve_global(import.mod_name(), import.name(), global_type)?;
                    self.globals.push(resolved_global);
                }
            }
        }

        Ok(())
    }

    fn add_functions<Iter: Iterator<Item = (usize, core::Func)>>(
        &mut self,
        functions: Iter,
        metadata: &RawModuleMetadata,
    ) -> Result<()> {
        for (type_idx, func) in functions {
            if type_idx >= metadata.types.len() {
                return Err(anyhow!("Function has invalid type index"));
            }

            self.functions
                .push(Rc::new(RefCell::new(core::WasmExprCallable::new(
                    metadata.types[type_idx].clone(),
                    func.clone(),
                ))));
        }
        Ok(())
    }

    fn add_tables<Iter: Iterator<Item = core::TableType>>(&mut self, tables: Iter) -> Result<()> {
        for table in tables {
            self.tables.push(Rc::new(RefCell::new(Table::new(table))));
        }

        Ok(())
    }

    fn add_memories<Iter: Iterator<Item = core::MemType>>(&mut self, memories: Iter) -> Result<()> {
        for memory in memories {
            self.memories
                .push(Rc::new(RefCell::new(Memory::new(memory))));
        }

        Ok(())
    }

    fn add_globals(&mut self, globals: impl Iterator<Item = core::GlobalDef>) -> Result<()> {
        for global in globals {
            let global_type = global.global_type().clone();
            let init_expr = global.init_expr();

            let results = evaluate_constant_expression(init_expr, self, 1)?;
            let global = Global::new(global_type, results[0])?;

            self.globals.push(Rc::new(RefCell::new(global)));
        }

        Ok(())
    }

    fn collect_single_export<T>(idx: usize, items: &Vec<std::rc::Rc<T>>) -> Result<std::rc::Rc<T>> {
        if idx >= items.len() {
            return Err(anyhow!("Export has invalid index"));
        }

        Ok(items[idx].clone())
    }

    fn collect_exports<Iter: Iterator<Item = core::Export>>(
        &mut self,
        exports: Iter,
    ) -> Result<()> {
        for core::Export { nm, d } in exports {
            match d {
                core::ExportDesc::Func(idx) => {
                    self.exports.insert(
                        nm,
                        ExportValue::Function(Self::collect_single_export(idx, &self.functions)?),
                    );
                }
                core::ExportDesc::Table(idx) => {
                    self.exports.insert(
                        nm,
                        ExportValue::Table(Self::collect_single_export(idx, &self.tables)?),
                    );
                }
                core::ExportDesc::Mem(idx) => {
                    self.exports.insert(
                        nm,
                        ExportValue::Memory(Self::collect_single_export(idx, &self.memories)?),
                    );
                }
                core::ExportDesc::Global(idx) => {
                    self.exports.insert(
                        nm,
                        ExportValue::Global(Self::collect_single_export(idx, &self.globals)?),
                    );
                }
            }
        }

        Ok(())
    }

    fn add_func_types(&mut self, func_types: Vec<FuncType>) -> Result<()> {
        self.func_types = func_types;
        Ok(())
    }

    fn pre_execute_validate(&self) -> Result<()> {
        if self.tables.len() > 1 {
            Err(anyhow!("Too many tables"))
        } else if self.memories.len() > 1 {
            Err(anyhow!("Too many memoryies"))
        } else {
            Ok(())
        }
    }

    fn initialize_table_element(&self, element: core::Element) -> Result<()> {
        if element.table_idx() >= self.tables.len() {
            Err(anyhow!("Table initializer table idx out of range"))
        } else {
            let table = &self.tables[element.table_idx()];
            let offset = self.evaluate_offset_expression(element.expr())?;

            let functions = element.func_indices();
            let functions: Result<Vec<_>> = functions
                .into_iter()
                .map(|idx| {
                    if *idx < self.functions.len() {
                        Ok(self.functions[*idx].clone())
                    } else {
                        Err(anyhow!("Function index out of range"))
                    }
                })
                .collect();
            let functions = functions?;

            table.borrow_mut().set_entries(offset, &functions);

            Ok(())
        }
    }

    fn initialize_table_elements<Iter: Iterator<Item = core::Element>>(
        &self,
        iter: Iter,
    ) -> Result<()> {
        for element in iter {
            self.initialize_table_element(element)?;
        }

        Ok(())
    }

    fn initialize_memory_data(&self, data: core::Data) -> Result<()> {
        if data.mem_idx() >= self.memories.len() {
            Err(anyhow!("Memory initializer mem idx out of range"))
        } else {
            let memory = &self.memories[data.mem_idx()];
            let offset = self.evaluate_offset_expression(data.expr())?;

            let data = data.bytes();

            memory.borrow_mut().set_data(offset, data)?;

            Ok(())
        }
    }

    fn initialize_memory<Iter: Iterator<Item = core::Data>>(&self, iter: Iter) -> Result<()> {
        for data in iter {
            self.initialize_memory_data(data)?;
        }

        Ok(())
    }

    fn evaluate_offset_expression(&self, expr: &impl InstructionSource) -> Result<usize> {
        let result = evaluate_constant_expression(expr, self, 1)?;

        match result[0] {
            StackEntry::I32Entry(i) => Ok(usize::try_from(i).unwrap()),
            _ => Err(anyhow!("Type mismatch in offset expression")),
        }
    }

    pub fn resolve_raw_module<Resolver: core::Resolver>(
        module: RawModule,
        resolver: &Resolver,
    ) -> Result<Module> {
        let mut ret_module = Self::new();
        ret_module.resolve_imports(module.imports.into_iter(), &module.metadata, resolver)?;
        ret_module.add_functions(
            module.typeidx.into_iter().zip(module.funcs.into_iter()),
            &module.metadata,
        )?;
        ret_module.add_tables(module.tables.into_iter())?;
        ret_module.add_memories(module.mems.into_iter())?;
        ret_module.add_globals(module.globals.into_iter())?;
        ret_module.collect_exports(module.exports.into_iter())?;
        ret_module.add_func_types(module.metadata.types)?;

        // Everything prior to this point is setting up the environment so that we
        // can start executing things, so make sure that everything is sane once we're
        // at that point.
        ret_module.pre_execute_validate()?;

        // The next step is to initialize the tables and memories.
        ret_module.initialize_table_elements(module.elem.into_iter())?;
        ret_module.initialize_memory(module.data.into_iter())?;

        // Finally, if there is a start function specified then execute it.
        if let Some(start) = module.start {
            if start >= ret_module.functions.len() {
                return Err(anyhow!("Start function not found"));
            }

            let start = ret_module.functions[start].clone();

            let mut stack = Stack::new();
            start.borrow().call(&mut stack, &mut ret_module)?;
        }

        Ok(ret_module)
    }
}

impl ConstantExpressionStore for Module {
    type GlobalRef = CellRefType<Global>;

    fn global_idx<'a>(&'a self, idx: usize) -> Result<Ref<'a, Global>> {
        if idx < self.globals.len() {
            Ok(self.globals[idx].borrow())
        } else {
            Err(anyhow!("Global index out of range"))
        }
    }
}

impl ExpressionStore for Module {
    type GlobalRefMut = CellRefMutType<Global>;
    type FuncTypeRef = RefType<FuncType>;
    type TableRef = CellRefType<Table>;
    type CallableRef = CellRefType<Callable>;
    type MemoryRef = CellRefType<Memory>;
    type MemoryRefMut = CellRefMutType<Memory>;

    fn global_idx_mut<'a>(&'a mut self, idx: usize) -> Result<RefMut<'a, Global>> {
        if idx < self.globals.len() {
            Ok(self.globals[idx].borrow_mut())
        } else {
            Err(anyhow!("Global index out of range"))
        }
    }

    fn func_type_idx<'a>(&'a self, idx: usize) -> Result<&'a FuncType> {
        if idx < self.func_types.len() {
            Ok(&self.func_types[idx])
        } else {
            Err(anyhow!("FuncType index out of range"))
        }
    }

    fn table_idx<'a>(&'a self, idx: usize) -> Result<Ref<'a, Table>> {
        if idx < self.tables.len() {
            Ok(self.tables[idx].borrow())
        } else {
            Err(anyhow!("Table index out of range"))
        }
    }

    fn callable_idx<'a>(&'a self, idx: usize) -> Result<Ref<'a, Callable>> {
        if idx < self.functions.len() {
            Ok(self.functions[idx].borrow())
        } else {
            Err(anyhow!("Callable index out of range"))
        }
    }

    fn mem_idx<'a>(&'a self, idx: usize) -> Result<Ref<'a, Memory>> {
        if idx < self.memories.len() {
            Ok(self.memories[idx].borrow())
        } else {
            Err(anyhow!("Memory index out of range"))
        }
    }

    fn mem_idx_mut<'a>(&'a mut self, idx: usize) -> Result<RefMut<'a, Memory>> {
        if idx < self.memories.len() {
            Ok(self.memories[idx].borrow_mut())
        } else {
            Err(anyhow!("Memory index out of range"))
        }
    }
}
