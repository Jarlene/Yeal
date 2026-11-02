# xai-codebase-graph

> 本文是 `xai-codebase-graph` crate 的工作原理详解（简体中文）。代码标识符、
> 文件路径与技术术语一律保留英文，便于与源码逐一对证。
> 文中每一条架构性论断都标注了它对应的真实文件 / 函数，可以打开源码核验。

`xai-codebase-graph` 是一个用 **tree-sitter query** 驱动的高性能代码图（code graph）
生成与查询 crate。它属于 `crates/codegen/xai-codebase-graph`，对外以 `xai_codebase_graph`
为 crate 名（见 `Cargo.toml` 的 `[package] name`，模块根在 `src/lib.rs`）。

## 目录

1. [概述：它能做什么、支持哪些语言](#1-概述它能做什么支持哪些语言)
2. [架构与数据流：从文件树到索引](#2-架构与数据流从文件树到索引)
3. [LanguageRegistry 与 TSLanguageConfig](#3-languageregistry-与-tslanguageconfig)
4. [索引数据结构与 SGIX 缓存格式](#4-索引数据结构与-sgix-缓存格式)
5. [增量更新与并发模型](#5-增量更新与并发模型)
6. [导航语义（goto-definition / goto-references）](#6-导航语义goto-definition--goto-references)
7. [扩展一种新语言的配方](#7-扩展一种新语言的配方)
8. [CLI 与基准工具](#8-cli-与基准工具)

---

## 1. 概述：它能做什么、支持哪些语言

### 1.1 定位与四大能力

`src/lib.rs` 顶部文档明确列出了这个 crate 提供的四件事：

| 能力 | 含义 | 主要入口 |
| --- | --- | --- |
| **Go-to-definitions** | 给定符号，找到它在哪些文件的哪一行被定义 | `Navigator::goto_definition*`、`ScopeGraphIndex::find_definitions*` |
| **Go-to-references** | 给定符号，找到它在哪些位置被引用 | `Navigator::goto_references*`、`ScopeGraphIndex::find_references*` |
| **Initial repository indexing** | 从零全量构建索引 | `IndexBuilder::build`（`src/manager/builder.rs`） |
| **Incremental reindexing** | 依据文件系统事件增量更新索引 | `IndexManager` / `FileEvent`（`src/index_manager.rs`） |

外加两条支撑性能力：

- **并行解析**：全量构建用 rayon 并行解析多个文件（见第 2 节）。
- **缓存**：索引以自定义二进制格式（magic bytes `"SGIX"`）落盘，二次启动直接
  `load_index` 加载而非重新扫描（见第 4 节）。

### 1.2 支持的语言族与扩展名归属

所有语言配置集中注册在 `LanguageRegistry::new()`（`src/languages/mod.rs`），
当前按以下顺序注册 **8 个** `TSLanguageConfig`（每个语言文件一个，语法依赖在
`Cargo.toml` 的 `[dependencies]` 中声明）：

| 配置文件 | 构造函数 | language_ids | file_extensions | tree-sitter 语法（Cargo.toml 版本） |
| --- | --- | --- | --- | --- |
| `src/languages/rust.rs` | `rust_lang()` | `Rust` / `rust` / `rs` | `rs` | `tree-sitter-rust 0.24.0` |
| `src/languages/ts.rs` | `ts_lang()` | `Typescript` / `TSX` / `typescript` / `tsx` | `ts`, `tsx` | `tree-sitter-typescript 0.23.2`（`LANGUAGE_TSX`，覆盖 TS/TSX 两种扩展） |
| `src/languages/javascript.rs` | `js_lang()` | `JavaScript` / `javascript` / `js` / `jsx` | `js`, `jsx` | `tree-sitter-javascript 0.25.0` |
| `src/languages/golang.rs` | `golang()` | `Go` / `go` | `go` | `tree-sitter-go 0.25.0` |
| `src/languages/python.rs` | `python_lang()` | `Python` / `python` / `py` | `py` | `tree-sitter-python 0.25.0` |
| `src/languages/java.rs` | `java_lang()` | `Java` / `java` | `java` | `tree-sitter-java 0.23`（**新增**） |
| `src/languages/c.rs` | `c_lang()` | `C` / `c` | `c`, `h`（**新增，拥有 `.h`**） | `tree-sitter-c 0.24`（**新增**） |
| `src/languages/cpp.rs` | `cpp_lang()` | `C++` / `cpp` / `cxx` | `cpp`, `cc`, `cxx`, `hpp`, `hh`, `hxx`（**新增**） | `tree-sitter-cpp 0.23`（**新增**） |

即“原有 5 家（Rust/TypeScript/TSX、JavaScript/JSX、Go、Python）+ 新增 3 家
（Java、C、C++）”。

### 1.3 `.h` 头文件归属决策

`.h` 后缀在 C 与 C++ 之间是历史歧义点。本 crate 的决策是：

- **`.h` 归 C 语言**：`src/languages/c.rs` 的 `file_extensions` 为 `["c", "h"]`，
  `for_extension("h")` 解析到 primary id 为 `"C"` 的配置。
- **C++ 使用 `.hpp` / `.hh` / `.hxx`**（以及源文件 `.cpp` / `.cc` / `.cxx`），
  `src/languages/cpp.rs` 的扩展列表中**刻意不含 `h`**。

`LanguageRegistry::new()`（`src/languages/mod.rs`）按 `configs` 向量顺序把每个扩展
插入 `by_extension: HashMap<String, Arc<TSLanguageConfig>>`。C 排在 C++ 之前注册，
且 C++ 配置本身不声明 `h`，因此 `h` 不会出现“被后注册配置覆盖”的静默覆盖。
这个单一归属不变量由集成测试强制执行：`tests/multilang_navigation.rs` 中的
`mlang_registry_single_owner_per_extension_and_families` 折叠所有配置的扩展集合，
断言 (a) 每个扩展恰好被一个配置拥有、(b) 同族扩展两两 `extensions_same_language`、
(c) `.h` → `C`、`.hpp` → `C++`、`extensions_same_language("c","h")` 为真而
`("h","hpp")` / `("c","cpp")` 为假。

> 为什么“同族扩展”这个概念重要：见第 6 节——查询结果按“上下文文件是否与结果
> 同语言”排序靠前，`.c` 文件里查 `distance` 时 `mathlib.c` 的定义会排在其它语言同名
> 符号前面。

---

## 2. 架构与数据流：从文件树到索引

### 2.1 目录结构速览

```
crates/codegen/xai-codebase-graph/
├── Cargo.toml
├── src/
│   ├── lib.rs                  # 模块声明 + 公共 re-export
│   ├── index_manager.rs        # 增量更新 actor（IndexManager/FileEvent/IndexCommand…）
│   ├── interner.rs             # StringInterner / StringId
│   ├── navigation.rs           # Navigator（位置→符号→定义/引用）
│   ├── languages/              # LanguageRegistry + 8 个语言配置
│   │   ├── mod.rs              # LanguageRegistry、compute_query_hash
│   │   ├── types.rs            # TSLanguageConfig、symbol_id_of
│   │   └── rust/ts/javascript/golang/python/java/c/cpp.rs
│   ├── manager/
│   │   ├── mod.rs              # 对外 re-export
│   │   ├── builder.rs          # IndexBuilder（全量并行构建）
│   │   ├── cache.rs            # CACHE_FILE_NAME/load_index/save_index/get_cache_path
│   │   └── lock.rs             # 跨进程工作区锁（try_lock/WorkspaceLockGuard）
│   ├── scope_graph/
│   │   ├── mod.rs              # build_scope_graph 等公共 API
│   │   ├── graph.rs            # ScopeGraph / ScopeGraphIndex / 序列化 / QueryVersion
│   │   ├── nodes.rs            # NodeKind/LocalDef/LocalImport/Reference/SymbolId
│   │   └── edges.rs            # EdgeKind
│   ├── types/                  # FileMeta/IndexStats/Position/Range/…（位置与元数据）
│   └── bin/                    # code_graph.rs（CLI）+ bench_index/bench_file_listing
└── tests/
    ├── memory_integration.rs   # 过滤器/批处理/缓存往返的集成测试
    └── multilang_navigation.rs # mlang_* 位置式导航 + 注册表不变量测试
```

### 2.2 全量构建（IndexBuilder）的数据流

入口是 `IndexBuilder::build(root_path)`（`src/manager/builder.rs`），链路如下：

**阶段 0 —— 收集文件列表**（`collect_files`）：

1. 优先 `collect_files_git`：用 `git2` 打开 `Repository::open(root)`，读 git index
   （`repo.index().iter()`），只保留 `registry.is_supported(path_str)` 的**被跟踪**
   文件。注释说明了原因：未跟踪文件通常是少数，且会在被创建时由 fsnotify 事件补上；
   对未跟踪文件做 statuses() 查询慢约 10 倍。
2. 非 git 仓库则回退 `collect_files_walk`：用 `ignore::WalkBuilder` 并行行走
   （`.hidden(skip_hidden)`、`.git_ignore/.git_global/.git_exclude(respect_gitignore)`，
   线程数 `min(num_threads, 12)`），对每个文件判断 `registry.is_supported(path)`。

**阶段 1 —— 并行解析 + 提取符号**（`build_fast` → `process_file_fast`）：

- rayon 线程池按 `num_threads`（默认 `num_cpus - 1`，见 `default_num_threads`）创建；
- 文件先按 `chunk_size`（默认 100）切块做 `par_chunks`，保证线程局部缓存局部性；
- `process_file_fast` 对单个文件依次过滤：
  - `registry.for_file_path(path)` 拿不到配置 → 直接 `None`（扩展不支持）；
  - `fs::metadata`：`len() == 0` 或 `len() > MAX_INDEXABLE_FILE_SIZE` → 跳过。
    `MAX_INDEXABLE_FILE_SIZE` 定义在 `src/index_manager.rs`：
    `pub const MAX_INDEXABLE_FILE_SIZE: u64 = 5 * 1024 * 1024;`（5 MiB，防御超大文件
    撑爆 tree-sitter AST）；
  - **二进制探测**：只前缀读取 8 KB 检查 `\0` 字节（同 git 的启发式，避免为发现
    “是二进制”而把整个文件读进内存）；
- 解析与 query 用**线程局部缓存** `PARSER_CACHE` / `QUERY_CACHE`（`thread_local!`，
  key 为语言 primary id），避免每文件重复创建 parser / 编译 query；
- 提取走 `extract_symbols_fast_inline`（与 `src/scope_graph/graph.rs` 导出的公开函数
  `extract_symbols_fast` 逻辑一致，见 2.3），产出 `FileSymbols { definitions,
  references, aliases, file_meta }`；
- 路径经 `xai_grok_paths::to_relative_path(root_path, path)` 转成**相对路径**再存，
  保证索引可移植。

**阶段 2 —— 顺序归并**（仍在 `build_fast`）：

- 为把“两阶段峰值内存”封顶，`build_fast` 按 `build_batch_size`（默认 5000 文件/批，
  且 clamp 到 ≥ `chunk_size`）分批：每批并行解析完成后立刻顺序 merge 进
  `ScopeGraphIndex`（`add_definition` / `add_reference` / `add_alias_arc` /
  `set_file_meta`），随后丢弃该批 `FileSymbols` 再处理下一批。
  函数注释明确对比了新旧做法：旧做法“先收集全部 FileSymbols 再归并”峰值 =
  O(总文件数) 符号 + 增长中的索引；新做法峰值 = O(batch) 符号 + 增长中的索引。
- 全部归并完后 `set_query_version(registry.compute_query_hash())`，再 `index.compact()`
  回收 Vec 倍增造成的超额容量（`ScopeGraphIndex::compact` 会顺带调用
  `StringInterner::shrink_to_fit`，见 `src/interner.rs`）。

### 2.3 查询捕获提取（capture extraction）

tree-sitter query 跑出来的捕获不会全部进入索引。三个提取函数逻辑一致，都只认
以下捕获名约定（详见第 3 节）：

- 前缀 `name.definition.` → 定义；
- 前缀 `name.reference.` → 引用；
- 恰好等于 `alias.original` / `alias.name` → 同一 match 内配对成 alias；
- 其余捕获（包括外层 `@definition.X` / `@reference.X` 标记捕获）**一律忽略**。

三个提取函数分布在三条路径上：

| 函数 | 位置 | 用在 |
| --- | --- | --- |
| `extract_symbols_fast_inline` | `src/manager/builder.rs` | 全量构建的并行路径（收集 `Vec<SymbolOccurrence>`） |
| `intern_symbols_directly` | `src/index_manager.rs` | 单文件增量重索引：符号名直接从 `&src[byte_range]` intern 进 `StringInterner`，零中间 `Arc<str>` 分配 |
| `extract_symbols_fast` | `src/scope_graph/graph.rs` | 公开 API（`lib.rs` re-export），同样只做轻量提取 |

另外还有一个“重型”通用路径 `scope_graph_from_definitions_query` /
`build_scope_graph`（`src/scope_graph/graph.rs` 与 `mod.rs`）：它会为单个文件构建
完整的 `ScopeGraph`（def/ref/import 节点带 `SymbolId`，见第 3.4 节），并返回
alias 对。它的 doc 注释明确说明轻量提取“~2-3x faster for indexing purposes”，
因此当前全量构建与增量路径都走轻量提取。

### 2.4 落库：ScopeGraphIndex

所有提取出的 `(符号名, 相对路径, 1-indexed 行号)` 汇聚进
`ScopeGraphIndex`（`src/scope_graph/graph.rs`）。它的内部结构见第 4 节。
`scope_graph/mod.rs` 说明 `ScopeGraph` 代表“单个源文件内的符号与其关系”，而
`ScopeGraphIndex` 是跨文件索引。

---

## 3. LanguageRegistry 与 TSLanguageConfig

### 3.1 LanguageRegistry（`src/languages/mod.rs`）

注册表三张结构：

- `configs: Vec<Arc<TSLanguageConfig>>` —— 全部语言配置（`all_configs()` 可读）；
- `by_extension: HashMap<String, Arc<TSLanguageConfig>>` —— 扩展 → 配置
  （`for_extension`、`for_file_path`、`is_supported`）；
- `by_id: HashMap<String, Arc<TSLanguageConfig>>` —— language id → 配置
  （`for_id`）。

关键方法：

- `for_extension(ext)` / `for_file_path(path)`：取扩展的 `.extension()` 查表；
- `extensions_same_language(ext1, ext2)`：同扩展直接 true；否则两个扩展都注册过时
  比较 `primary_language_id()` 是否相等（“同族扩展”判定的唯一来源，被第 6 节的排序
  与测试使用）；
- `compute_query_hash()`：见第 4.3 节。

### 3.2 TSLanguageConfig 的字段（`src/languages/types.rs`）

```rust
pub struct TSLanguageConfig {
    language_ids: Vec<String>,        // 主 id 在第一个，如 ["Java", "java"]
    file_extensions: Vec<String>,     // 如 java 配置是 ["java"]
    namespaces: Vec<Vec<String>>,     // 符号种类(namespace)表，如 java 的 ["class","interface",...]
    file_definition_queries: String,  // 唯一的 query 文本（定义+引用+alias 全部写在这里）
    grammar: GrammarFn,               // type GrammarFn = fn() -> tree_sitter::Language;
}
```

构造器是 `TSLanguageConfig::new(language_ids, file_extensions, namespaces,
file_definition_queries, grammar)`。访问器：`primary_language_id()`（返回
`language_ids[0]`，空则 `"unknown"`）、`language_ids()`、`file_extensions()`、
`namespaces()`、`file_definition_queries()`、`language()`。

**每个语言一份 query 的契约**：`compile_query()`（`types.rs`）把整个
`file_definition_queries` 字符串用 `tree_sitter::Query::new(&self.language(), &...)`
编译成**一个** query。也就是说，“定义模式、引用模式、alias 模式”都放在同一个字符串
里，没有按用途拆分文件——一个语言 = 一个 config = 一份 query 文本。

### 3.3 捕获名（capture name）契约

因为索引只认特定捕获名（见 2.3），query 模式里的捕获名必须遵守下面的固定语法。
符号种类（kind）以点分字段的第三段出现，例如 `@name.definition.function` 的 kind
是 `function`：

| 捕获名 | 语义 | 谁消费 |
| --- | --- | --- |
| `@name.definition.<kind>` | 打在 **name 节点自身**上——真正被索引的“定义出现处”（取该节点的文本与位置） | 索引提取（def） |
| `@definition.<kind>`（外层标记） | 打在整个匹配外围的**标记捕获**，表达“这段是一个 `<kind>` 定义” | 目前提取器不读取，属于风格化/文档化外层标记 |
| `@name.reference.<kind>` | 打在 **name 节点自身**上——被索引的“引用出现处” | 索引提取（ref） |
| `@reference.<kind>`（外层标记） | 同 `@definition.X`，仅标记 | 目前提取器不读取 |
| `@alias.original` / `@alias.name` | **同一 match 内成对**：`alias.name` 是别名、`alias.original` 是原名；二者缺一则该 match 不产生 alias | 提取器按 match 配对 → `add_alias(alias, original)` |

> 为什么外层 `@definition.X` 是“风格化”？看提取器代码即可证实：三个提取函数都只
> 匹配 `starts_with("name.definition.")` / `starts_with("name.reference.")` 以及精确的
> `alias.original` / `alias.name`；`@definition.class` 这类字符串不落入任何分支，被静默
> 忽略。因此模式中写不写外层标记不影响当前索引结果——但它保留了“这段匹配是什么”
> 的意图信息，并在类型感知解析路径（`scope_graph_from_definitions_query`）中作为模式
> 结构的一部分出现。

**示例 1 —— `src/languages/rust.rs`（函数定义）**：

```
(function_item
    name: (identifier) @name.definition.function) @definition.function
```

`@name.definition.function` 打在 `fn foo` 的 `foo` 这个 `identifier` 节点上；整个
`function_item` 上另有外层标记 `@definition.function`。

**示例 2 —— 新语言之一 `src/languages/java.rs`（方法定义）**：

```
(method_declaration
    name: (identifier) @name.definition.method) @definition.method
```

Java 语言配置中类似的 kind 还有 `class` / `interface` / `enum` / `annotation` /
`record` / `constructor` / `field` / `variable` / `package` / `import` / `type`。

**示例 3 —— 引用捕获（`src/languages/c.rs`，函数调用）**：

```
(call_expression
    function: (identifier) @name.reference.function) @reference.function
```

**示例 4 —— alias 配对（`src/languages/cpp.rs`，`using New = Old;`）**：

```
(alias_declaration
    name: (type_identifier) @alias.name
    type: (type_descriptor
        type: (type_identifier) @alias.original))
```

（Rust 与 TS 的 `use Foo as Bar` / `import { Foo as Bar }` alias 模式写法类似，
见 `src/languages/rust.rs` 的 `use_as_clause` 与 `src/languages/ts.rs` 的
`import_specifier`。）

### 3.4 namespaces 表、SymbolId 与“未知 kind 优雅降级”

- `namespaces: Vec<Vec<String>>` 目前所有语言都只用一个分组（`vec![vec![...]]`），
  例如 Java 的 12 个 kind 全在 `namespaces[0]` 里（见 `java.rs` 构造参数）。
- `symbol_id_of(symbol_type)`（`types.rs`）线性扫描 namespaces，返回
  `Option<SymbolId>`，其中 `SymbolId { namespace_idx, symbol_idx }`
  （`src/scope_graph/nodes.rs`）。即“kind 字符串 → 坐标”。
- 当 `symbol_id_of` 查不到（kind 不在表中）时返回 **`None`**——调用方
  `scope_graph_from_definitions_query`（`graph.rs`）只是把 `None` 作为 `symbol_id`
  塞进 `LocalDef`/`Reference`，**不会报错或丢弃**。类型感知解析
  `ScopeGraph::insert_ref`（`graph.rs`）中 `(Some(d), Some(r))` 且 namespace 不同
  才不连边；`symbol_id` 为空的一方（`None`）“属于所有 namespace”，因此照常连边。
  这就是“未知 kind 优雅降级”的具体语义。
- 需要澄清的一点：**生产索引路径（全量构建 + 增量重索引）并不携带 SymbolId**，
  它只存“名字 + 行”。`SymbolId` 与 namespace 感知解析属于
  `build_scope_graph` / `scope_graph_from_definitions_query` 提供的完整 per-file
  ScopeGraph API 的机制。

### 3.5 语言文件的测试惯例

目前只有三个新语言文件在文件末尾带 `#[cfg(test)] mod tests`（Java/C/C++）。
例如 Java 的
`java_lang_query_and_registry`：用 `src/Main.java` 这个路径从注册表解析出配置 → 断言
`primary_language_id() == "Java"` → `compile_query().expect(...)` 证明 query 能在该
语法下编译 → 断言扩展表含 `java` → 对每一个 query 会发出的 kind token 断言
`config.symbol_id_of(kind).is_some()`（防止 kind 表与 query 脱节）。
C 的 `c_lang_query_and_registry` 类似并额外断言 `.c` 与 `.h` 都由 primary id `"C"`
拥有。C++ 的 `cpp_lang_query_and_registry` 额外遍历 `query.capture_names()`，把所有
`name.definition.*` / `name.reference.*` 的 kind 段逐一与 `namespaces` 核对。
C/C++ 的 `*_cross_file_navigation` 测试则用 `IndexBuilder` + `Navigator` 跑真实
夹具（`mathlib.h/mathlib.c/main.c`、`geometry.hpp/geometry.cpp/main.cpp`），断言
定义/引用的精确 (文件, 行)。

---

## 4. 索引数据结构与 SGIX 缓存格式

### 4.1 StringInterner（`src/interner.rs`）

`ScopeGraphIndex` 用 `StringInterner` 对所有**文件路径**与**符号名**做去重：

- `arena: Vec<u8>` —— 全部字节串放进一块连续缓冲区（提升 cache locality）；
- `lookup: HashMap<u64, SmallVec<[StringId; 1]>>` —— 64-bit hash → 候选 id 列表
  （`nohash_hasher::BuildNoHashHasher` 因 key 已 hash 不再二次 hash）；碰撞时逐字节比对
  真串（两级查找）；
- `offsets: Vec<(u32, u16)>` —— `StringId` → arena 内 `(start, len)`，因此
  `StringId` 是 u32 句柄。

`intern` / `intern_bytes` 平均 O(1)；`StringId` 的 doc 说 u32 可容纳约 40 亿唯一串。
它支持任意字节（非 UTF-8 路径也能存），`from_parts(arena, offsets)` 可从序列化数据
重建 lookup 表。内存收益写在 `ScopeGraphIndex` 的 doc 注释里：路径 + 符号统一
去重把大仓库的内存占用从约 1 GB 降到约 100 MB 量级。

### 4.2 ScopeGraphIndex 的核心 map（`src/scope_graph/graph.rs`）

`ScopeGraphIndex` 定义（字段都是 `pub(crate)`，见 `graph.rs`）：

```rust
pub struct ScopeGraphIndex {
    interner: StringInterner,                                  // 路径/符号去重
    graphs: HashMap<StringId, ScopeGraph>,                     // 文件 → per-file 图（不入缓存）
    definitions: HashMap<StringId, Vec<(StringId, u32)>>,      // 符号 → [(文件, 行)]
    references: HashMap<StringId, Vec<(StringId, u32)>>,       // 符号 → [(文件, 行)]
    aliases: HashMap<StringId, StringId>,                      // alias → original
    reverse_aliases: HashMap<StringId, AHashSet<StringId>>,    // original → 别名集合
    file_meta: HashMap<StringId, FileMeta>,                    // 文件 → (size, mtime)
    query_version: QueryVersion,                               // Legacy | Version(u64)
    file_to_defs: HashMap<StringId, AHashSet<StringId>>,       // 文件 → 在本文件有定义的符号
    file_to_refs: HashMap<StringId, AHashSet<StringId>>,       // 文件 → 在本文件有引用的符号
}
```

要点：

- **`definitions` / `references` 的值是 `(StringId, u32)` 行号二元组**：字段注释说明
  u32（而非 usize）让每条记录从 16 字节（64-bit 上的 `(StringId, usize)`）降到 8 字节。
- **行号是 1-indexed**，写入时统一从 0-indexed 的行坐标转换（`add_file` 里
  `range.start_line() + 1`；轻量提取里 `start_position().row + 1`）。
  超过 `u32::MAX` 的行号**饱和**而非回绕（`add_definition_with_path_id` /
  `add_reference_with_path_id`，有单测 `test_line_number_overflow_saturates` 钉死）。
- **`file_meta` 存 `FileMeta { size, mtime_secs, mtime_nanos }`**
  （`src/types/mod.rs`）；`FileMeta::is_stale(path)` 把缓存值与磁盘 metadata 比较，
  不一致或文件消失即视为 stale——这是缓存文件级校验的最小单元。
- **反向索引 `file_to_defs` / `file_to_refs`** 使“移除一个文件”变成
  O(该文件涉及符号数) 而不是 O(全索引符号数)：`remove_file`（`graph.rs`）先
  `file_to_defs.remove(path_id)` 拿到受影响符号集合，再对每个符号在
  `definitions` 的 location 列表里 `retain` 掉该 path，空列表则整键删除；
  `rename_file` 同理就地改 `path_id`。该反向索引**不落盘**，`read_from` 加载后从
  definitions/references 重建（`graph.rs` 中 `read_from` 尾部）。
- **alias 查询语义**（统一查询 unified lookup）：`add_alias(alias, original)` 同时写
  `aliases` 与 `reverse_aliases`；`find_definitions` 在直接命中之外还走
  `aliases.get(symbol_id)` 找原名定义；`find_references`（及其 `_with_names` 变体）除
  原名引用外还遍历 `reverse_aliases` 找“该符号所有别名的引用”。
- `graphs`（per-file `ScopeGraph`）**不参与序列化**——`write_to` 末尾注释明确
  “Note: graphs are not serialized”。导航与查询全部基于上面的 occurrence map。

### 4.3 SGIX 缓存格式与 query_version

**常量**（`graph.rs`）：

```rust
pub const SCOPE_GRAPH_INDEX_MAGIC: &[u8; 4] = b"SGIX";   // ScopeGraphIndex
pub const SCOPE_GRAPH_INDEX_VERSION: u16 = 1;
```

自定义二进制格式按序写入（`ScopeGraphIndex::write_to`）：

1. magic `"SGIX"`（4 字节）+ `SCOPE_GRAPH_INDEX_VERSION`（u16 LE）；
2. interner arena（长度 u32 + 裸字节）；
3. interner offsets（个数 u32 + 每个 `(start u32, len u16)`）→ 加载时
   `StringInterner::from_parts` 重建；
4. definitions：每个符号 `(symbol_id u32, location 数 u32, 每个 (path_id u32,
   line u32))`；
5. references：同 definitions；
6. aliases：每对 `(alias_id u32, original_id u32)`；
7. file_meta：每个 `(path_id u32, size u64, mtime_secs i64, mtime_nanos u32)`；
8. query_version：1 字节 tag（`0` = Legacy，`1` = 后随 u64 hash）。

`ScopeGraphIndex::load` 先读 4 字节 magic，不匹配 `"SGIX"` 就返回 `Ok(None)` 表示
“旧 bincode 时代格式”，由 `src/manager/cache.rs` 的 `load_index` 转成
`CacheError::LegacyFormat` 提示调用者重建。

**query_version 如何保证“语言配置一变，旧缓存自动作废”**：

- `QueryVersion` 枚举（`graph.rs`）：`Legacy`（默认，表示无法确定旧缓存用什么 query
  建的）或 `Version(u64)`。`needs_rebuild(current)`：`Legacy` 恒 true，版本不同则 true。
- `LanguageRegistry::compute_query_hash()`（`languages/mod.rs`）：
  1. 把 `configs` 按 `primary_language_id()` 排序（确定性顺序）；
  2. 依次把每个配置的 `primary_language_id()` 与整个
     `file_definition_queries()` 文本 hash 进同一个 `DefaultHasher`。
  所以**任何语言 query 文本的改动都会改变 hash**；同理，向注册表**新增语言
  （Java/C/C++）会改变配置集合**，hash 必然变化。
- `IndexBuilder::build` / `build_fast` 结束时 `set_query_version(compute_query_hash())`
  （`builder.rs`），把版本 stamp 写进缓存。
- `IndexManager::spawn`（`index_manager.rs`）加载缓存后比较
  `idx.needs_query_rebuild(current_query_version)`：不一致（含 Legacy）就打日志
  “Query version mismatch, rebuilding index”并 `build_fresh_index` 重建；
  一致则“Loaded index from cache (will validate in background)”，再启动后台校验
  （见第 5 节）。这也是为什么**升级到支持 Java/C/C++ 的新版本后，旧仓库缓存会自动
  重建**而不是把缺失语言的旧数据继续当有效数据用。

> 附带说明：CLI（`src/bin/code_graph.rs` 的 `load_or_build_index`）与裸
> `cache.rs::load_index` 不查 query hash（只做格式识别 + 存在性）；query 版本校验是
> `IndexManager` 启动路径的职责。集成测试 `test_index_includes_query_version` 等
> （`index_manager.rs` 测试区）覆盖了该机制。

---

## 5. 增量更新与并发模型

### 5.1 单写者 actor：IndexManager（`src/index_manager.rs`）

设计要义（模块 doc 有 ASCII 图）：`IndexManager` 把 `ScopeGraphIndex` 放在**自己专属
的后台线程**里顺序处理命令，从而**不需要 `Arc<Mutex<Index>>`**。对外暴露的是
`IndexManagerHandle`——它只持有 `command_tx: crossbeam::channel::Sender<IndexCommand>`，
所有操作都变成往 channel 发消息 + 用 `tokio::sync::oneshot` 收响应。

约束与生命周期：

- `IndexManager::spawn(config)`（推荐入口，非阻塞启动）会先做
  `dunce::canonicalize`，然后查全局 `ACTIVE_MANAGERS: DashMap<PathBuf,
  Weak<IndexManagerHandle>>`——保证**每个 workspace 每进程最多一个 manager**
  （弱引用让失效句柄自动清理）。
- 后台线程名为 `"index-manager"`，启动时先“加载缓存或全量构建”（慢操作，期间查询在
  channel 里排队），随后进入 `run_loop()`。
- `run_loop` 在 channel 断开（所有 handle 被 drop，actor 用 `Weak` 保存 handle
  所以不会自我续命）时退出并做最后一次 `save_cache()`。
- `index: Arc<ScopeGraphIndex>`：变更走 `Arc::make_mut`——**没有活着的 snapshot 时
  零成本，有 snapshot 时 COW 克隆一次**；`GetSnapshot` 命令直接 `Arc::clone` 发出去
  （doc 注释：clone 零成本 / 无变更在途时）。
- `IndexCommand` 是命令全集（`index_manager.rs`）：`FileEvent` / `FileEventBatch` /
  `Rebuild` / `GetSnapshot` / `GotoDefinition` / `GotoReferences` /
  `FindDefinitions` / `FindReferences` / `BackgroundRefresh` / `GetFileCount` /
  `GetStats` / `GetQueryVersion` / `HasDefinition` / `Shutdown`。
  Handle 上的“轻量查询”`get_file_count()`、`get_stats()`、
  `has_definition_blocking()`、`get_query_version()` 都只发小命令拿小值，
  **不克隆整棵索引**（lib.rs 文档强调“Prefer these over get_snapshot() in hot paths”）。

### 5.2 事件合并（CoalescedEvents）

`process_command_coalesced`（`run_loop` 的每个命令入口）：收到一个 file event 后，会
用 `try_recv()` **把 channel 里已排队的事件全部 drain 出来**，逐条喂给
`CoalescedEvents`（一个 `HashMap<PathBuf, FileEventKind>`）；如果 drain 途中遇到非
文件命令，就先 `apply_coalesced` 冲刷已合并事件、再对该命令深度 1 递归处理。

`CoalescedEvents::insert` 的合并语义（代码 + 单测名可对证）：

| 现状 (prev) | 新事件 (kind) | 结果 |
| --- | --- | --- |
| `Created` / `Modified` | `Removed` | **整条取消**（`e.remove()`）——文件刚改又删，无事可做 |
| `Removed` | `Created` / `Modified` | 变 **`Created`**——文件被替换，需重新索引 |
| 其它（含 `Created`+`Modified`、连续 `Modified`） | —— | **last-writer-wins** 覆盖为最新 kind（map 键去重 ⇒ 多次 `Modified` 合并为一次） |
| `Renamed`（`paths.len() >= 2`） | —— | 在 `add` 里先**拆成两半**：`paths[0]` → `Removed`，`paths[1]` → `Created` |

对应测试：`test_coalesce_create_then_remove_cancels`、
`test_coalesce_modify_then_remove_cancels`、`test_coalesce_remove_then_create_is_created`、
`test_coalesce_remove_then_modify_is_created`、`test_coalesce_multiple_modify_is_single`、
`test_coalesce_rename_decomposes`、`test_coalesce_rename_then_remove_target_cancels_target`、
`test_coalesce_rename_then_modify_target` 等（均在 `index_manager.rs` 测试区）。

`apply_coalesced` 逐条应用：

- `Created` / `Modified` → `should_index(path, root)`（= 扩展受支持 且 不在隐藏目录下，
  见 `is_under_hidden_dir`）通过才 `reindex_file`；
- `Removed` → 同样过 `should_index` 才 `remove_file`（隐藏目录/不支持扩展的文件本来
  就没进索引，删无可删）；
- `Renamed`（API 层残留的单路径形态）→ `reindex_file`；
- 每应用一条 `updates_processed += 1`；
- 缓存保存按 `CACHE_SAVE_INTERVAL_SECS = 60` 秒节流（`last_cache_save`），防止事件
  洪峰时反复写盘阻塞。

### 5.3 单文件重索引与删除

`reindex_file(path)`（`index_manager.rs`）步骤：

1. 转相对路径后先 `remove_file`（清掉旧条目）；
2. 依次做防御：无语言配置直接 return；`metadata.len()==0 || > MAX_INDEXABLE_FILE_SIZE`
   直接 return；`is_binary_file`（8 KB 前缀查 `\0`）直接 return；`fs::read` 失败 return。
   doc 注释：读/解析失败（瞬时错误、二进制、超大）时该文件“保持缺席”，等下次成功
   再进索引；
3. 用 `parser_cache` / `query_cache`（key 为语言 id）复用 parser 与已编译 query；
   query 编译失败时与全量构建一致地落到**空 query 兜底**
   （`compile_query().unwrap_or_else(|_| tree_sitter::Query::new(&lang, "").expect(...))`）；
4. `intern_symbols_directly` 把符号名直接从源字节区间 intern 进索引并写
   def/ref/alias（无效 UTF-8 的区间跳过，不做 lossy 替换）；
5. `set_file_meta`（复用第 2 步的 metadata，避免重复 stat）。

`remove_file` / `rename_file` 在 `ScopeGraphIndex` 上实现（`graph.rs`），利用反向索引
做到 O(该文件符号数)（见 4.2）。

### 5.4 后台校验与跨进程锁

`IndexManager::spawn` 从缓存加载成功时（`needs_background_validation == true`）会再拉
一个名为 `"index-bg-refresh"` 的后台线程跑自由函数 `background_index_refresh`
（`index_manager.rs`，`src/index_manager.rs` 靠近文件尾部）：

1. **加锁去重**：先 `try_lock(root, IndexOperation::BackgroundRefresh)`；拿不到
   （别的进程正在做）直接返回“Skipping background refresh”。
2. **阶段 1（并行 stat）**：对缓存的 `(path, FileMeta)` 列表用
   `FileMeta::is_stale`（rayon `par_iter` + fold/reduce）筛出 `stale`（变了）与
   `deleted`（消失了）两组。
3. **阶段 2（发现新文件）**：`ignore::WalkBuilder` 走一遍文件系统，过滤出受支持、
   且不在缓存集合里的文件作为 new files。
4. 通过 `Weak<IndexManagerHandle>` 升级成功后发 `IndexCommand::BackgroundRefresh`，
   由 actor 线程 `process_background_refresh` 统一执行（先删 deleted 再重索引
   stale+new，最后 `save_cache()`）。若 manager 已退出（Weak 升级失败）就丢弃结果。

锁实现在 `src/manager/lock.rs`：

- `IndexOperation` 四种：`Load`（**共享读锁**）、`Save` / `Build` /
  `BackgroundRefresh`（**排他**），各自有 stale 超时（120s/120s/600s/300s）；
- 两层：进程内 `IN_MEMORY_LOCKS: DashMap<PathBuf, InMemoryLockState>` 做同进程互斥；
  排他操作额外写**锁文件**（路径 = 缓存文件换扩展名，即
  `<cache>.with_extension("lock")`，内容是 `operation=.../pid=.../started=...`）做
  跨进程协调；
- 陈旧检测：锁文件存在但 (a) 年龄超过该操作的 stale_timeout 或 (b) 持有进程已死
  （unix 下 `libc::kill(pid, 0)`），即可接管；
- `WorkspaceLockGuard` 析构时释放进程内锁并删除锁文件。测试覆盖排他/共享互斥、
  不同 workspace 独立、锁文件创建等（`lock.rs` 测试区）。

缓存读写层在 `src/manager/cache.rs`：`CACHE_FILE_NAME = ".goto_index.bin"`、
`get_cache_path(root) = root.join(".goto_index.bin")`、`save_index_async` 用后台线程
异步写盘。

---

## 6. 导航语义（goto-definition / goto-references）

### 6.1 Navigator API（`src/navigation.rs`）

```rust
pub struct Navigator { index: Arc<ScopeGraphIndex>, registry: LanguageRegistry }
```

构造 `Navigator::new(index)` 接受任何能 `Into<Arc<ScopeGraphIndex>>` 的东西（自有索引
或 `IndexManagerHandle::get_snapshot()` 拿到的共享快照都行）。主要方法：

- `get_symbol_at_position(file_path, row, col) -> Result<String, NavigationError>`：
  位置 → 符号名；
- `goto_definition(file_path, row, col)`、`goto_references(file_path, row, col,
  include_definition)`：位置式导航；
- `goto_definition_by_name(symbol, context_file)`、
  `goto_references_by_name(symbol, context_file, include_definition)`：按名直查，
  不需要文件存在（`tests/multilang_navigation.rs` 大量使用）。
- `index()` / `index_mut()`（`Arc::make_mut` COW）。

所有 API 的 **row/col 都是 1-indexed**（`Location` 的 doc 注释）。结果 `Location`
包含 `path`（相对索引根）、`line`（1-indexed）、`symbol`（可选，引用命中的别名原名）。

### 6.2 位置 → 符号：最小命名节点 + identifier-like 白名单

`get_symbol_at_position`（navigation.rs）与 `IndexManager::get_symbol_at_position`
（index_manager.rs）实现完全同构：

1. `row == 0 || col == 0` 视为越界（`PositionOutOfBounds` / `NoSymbolAtPosition`）；
2. `fs::read` 读文件（文件需在磁盘上）；
3. `registry.for_file_path` 取语言配置，解析成语法树；
4. 1-indexed 转 0-indexed：`Point::new(row - 1, col - 1)`；
5. `find_smallest_named_node_at_point(root, point)`：递归下钻，找包含该点的**最小
   命名节点**；子节点递归返回结果时优先接受“看起来像标识符”的节点；
6. **最终只接受 identifier-like 的节点**，否则返回
   `NoSymbolAtPosition`——把鼠标点在一行代码中间时不会把 `method_declaration` 整段
   当成符号名。

这个 identifier-like 判定就是两处 `is_identifier_like` 函数。**`src/navigation.rs`
（约 385 行附近）与 `src/index_manager.rs`（约 1650 行附近）各有一份完全相同的白名单，
且都带着互相提醒的 “Must stay in sync with is_identifier_like in ...” doc 注释**：

```rust
kind == "identifier"
    || kind == "type_identifier"
    || kind == "property_identifier"
    || kind == "field_identifier"
    || kind == "namespace_identifier" // C++
    || kind == "shorthand_property_identifier"
    || kind == "shorthand_property_identifier_pattern"
    || kind == "attribute"            // Python
    || kind == "package_identifier"  // Go
```

其中 `namespace_identifier` 是后来为 C++ 支持而补进白名单的（namespace 定义名与
`geometry::norm(...)` 限定作用域名都落在该 kind 上）。清单覆盖了 Java 的
`identifier`/`type_identifier`、C 的 `identifier`/`type_identifier`/
`field_identifier`、C++ 追加 `namespace_identifier` 等各 `@name.*` 捕获会落上去的
节点 kind。证据性测试：

- `navigation.rs` 测试区的 `evidence_name_capture_node_kinds_java_c_cpp`：用真实语法
  解析 Java/C/C++ 代表性片段、跑真实编译的 query、`eprintln!` 打印每个 `@name.*`
  捕获落在的 node kind（断言只做计数，可用 `--nocapture` 观察 kind 集合）；
- `tests/multilang_navigation.rs` 的 `mlang_java_class_and_method_names_resolve_from_position`、
  `mlang_c_function_and_macro_names_resolve_from_position`、
  `mlang_cpp_namespace_and_class_names_resolve_from_position`：对单文件夹具
  用 `locate()` 找到名字出现位置再 `goto_definition(path,row,col)`，断言符号名与
  定义 (文件, 行)。这些测试直接验证“点 Java 类名、C 函数名/宏名、C++ namespace/
  class 名都能解析到定义”。

### 6.3 定义/引用查询与“同语言优先”排序

位置解析出符号名后，查表走 `ScopeGraphIndex` 的 smart 变体：

- `find_definitions_smart(symbol, context_file, language_registry)` 与
  `find_references_smart(...)`（`src/scope_graph/graph.rs`）：在“直接命中 + alias 展开”
  基础上，把结果按 `context_file` 的扩展名与结果文件扩展名是否**同语言**排序——
  `extensions_same_language` 先用精确相等（`.c` == `.c`），否则比对
  `LanguageRegistry` 中两个扩展的 `primary_language_id()`。
  同语言的排前面（`Ordering::Less`），不同语言的排后面，组内按路径字典序稳定。
  例如在 `main.c` 中 goto-definition `distance`，`mathlib.c` 里的定义会排在其它同名
  符号之前；引用列表同理。
- `find_references_smart` 底层是 `find_references_with_names`，返回
  `(matched_symbol, path, line)` 三元组——`matched_symbol` 让“通过别名命中”的
  引用也能展示实际命中的名字。

`Navigator` 的 `goto_definition` / `goto_references` 与 `IndexManager` 的
`handle_goto_definition` / `handle_goto_references`（`index_manager.rs`）调用的就是
上述 smart 查询；`include_definition=true` 时把定义位置并到结果头部并去重。

### 6.4 单测与集成测试如何验证导航语义

- 各语言文件里的 `*_cross_file_navigation`（Java/C/C++ 各有）：双文件夹具 + 
  `goto_definition_by_name` / `goto_references_by_name`，如 Java 断言 `Greeter`
  定义在 `Foo.java:3` 与构造器 `Foo.java:6`、`greet` 定义 `Foo.java:10` 而调用点
  `Main.java:6` 是引用、`new Greeter(...)`（`Main.java:5`）是 `Greeter` 引用。
- `navigation.rs` 里更早的 TypeScript/JSX 测试覆盖解构、React 依赖数组引用等细节。
- `tests/memory_integration.rs` 覆盖二进制/隐藏目录/超大文件过滤、缓存往返、
  `IndexManager` 事件处理等端到端行为。

---

## 7. 扩展一种新语言的配方

本节以本仓库加入 Java/C/C++ 时实际走过的流程为蓝本，写成可照抄的
step-by-step。**完成态参考**：`src/languages/java.rs`、`src/languages/c.rs`、
`src/languages/cpp.rs` 就是三个已合入的工作示例。

### 步骤 0：加 tree-sitter 语法依赖

在 `crates/codegen/xai-codebase-graph/Cargo.toml` 的 `[dependencies]` 加一行，例如
Java 加的是 `tree-sitter-java = "0.23"`，C 加 `tree-sitter-c = "0.24"`，C++ 加
`tree-sitter-cpp = "0.23"`。tree-sitter 运行时本体已是 `tree-sitter = "0.25.10"`。

### 步骤 1：新建 `src/languages/<lang>.rs` 并实现 `TSLanguageConfig`

函数签名与既有文件一致（如 `pub fn java_lang() -> TSLanguageConfig`），四个参数：

1. **language_ids**：主 id 放第一个（如 `["Java", "java"]`、`["C", "c"]`、
   `["C++", "cpp", "cxx"]`）——`primary_language_id()` 与 `compute_query_hash` 的
   排序都依赖它；
2. **file_extensions**：声明归属的扩展名集合。**决策 .h 归属时先想好**：
   `.h` 归 C（`["c","h"]`），C++ 用 `["cpp","cc","cxx","hpp","hh","hxx"]` 并
   **不要**把 `h` 放进去——`LanguageRegistry::new()` 的 `by_extension` 是按配置顺序
   逐个插入的，若两个配置争同一个扩展，后注册者会**静默覆盖**先注册者；这个隐患由
   `tests/multilang_navigation.rs` 的 `mlang_registry_single_owner_per_extension_and_families`
   兜底（它断言每个扩展恰好一个归属、同族两两同语言）；
3. **namespaces**：一个 `Vec<Vec<String>>`，列出 query 里会用到的全部 kind token
   （定义 kind + 引用 kind）。它有两个消费者：`symbol_id_of` 的查找表，以及
   `java_lang_query_and_registry` 式测试里“每个 kind 都能 resolve”的断言底表；
4. **file_definition_queries**：唯一一份 query 文本。按第 3.3 节的捕获名契约写
   定义/引用/alias 三类模式；`grammar` 闭包如 `|| tree_sitter_java::LANGUAGE.into()`。

### 步骤 2：在 `src/languages/mod.rs` 注册

改两处：文件顶部加 `mod java;`（私有模块）与 `pub use java::java_lang;`；在
`LanguageRegistry::new()` 的 `configs` 向量里按顺序 `Arc::new(java_lang())`。
顺序影响 `by_extension` 的插入顺序与 `compute_query_hash` 的排序键，但只要有单主
归属（见 `tests/multilang_navigation.rs`）就不存在覆盖问题。

### 步骤 3：写 query 时对照 node-types.json，先静态保证可编译

**注意**：query 编译失败不会被运行时发现——全量构建（`builder.rs`
`process_file_fast`）与增量重索引（`index_manager.rs` `reindex_file`）对
`compile_query()` 都做 `unwrap_or_else(|_| Query::new(&lang, ""))` **空 query 静默兜底**。
也就是说，只要模式里有一个 node kind 或 field 名拼错，索引会“成功”地空手而归，
没有任何报错。所以每写一个模式都应到对应语法 crate 的 `node-types.json`
（或其 `tags.scm` / `highlights.scm`）核对节点与字段名。Java 有一个真实案例：
tree-sitter-java 0.23.5 中 `package_declaration` / `import_declaration` 是**无字段**
节点，它们的名字是普通直接子节点，因此 Java 配置用了“无字段、锚定直接子节点”的
写法，例如：

```
(package_declaration
    (scoped_identifier
        name: (identifier) @name.definition.package)) @definition.package
```

（`name:` 挂在有字段的 `scoped_identifier` 上；含 `import` 的对称模式同理，捕获
`@name.reference.import`。见 `java.rs` 中带注释的两组模式。）

### 步骤 4：修两处 `is_identifier_like` 白名单（如需要）

`get_symbol_at_position` 最终只接受白名单内的节点 kind。新增语法若让 `@name.*`
落在清单之外的 kind（例如 C++ 的 namespace 名是 `namespace_identifier`），位置式
导航会静默 `NoSymbolAtPosition`。**两个文件要同步改**：
`src/navigation.rs` 与 `src/index_manager.rs` 的 `is_identifier_like` 函数（各有一份、
互有 keep-in-sync 注释）。可以先抄第 6.2 节那个证据测试的思路：跑一遍真实 query，
`eprintln!` 打印 `@name.*` 落在的 kind，再决定白名单加什么。

### 步骤 5：写测试（照抄三个新语言文件的现成范式）

每个语言文件自带的 `#[cfg(test)] mod tests` 通常有两个测试：

1. `*_lang_query_and_registry`：`registry.for_extension` 能解析、`primary_language_id`
   正确、`compile_query().expect(...)` 为 Ok、扩展集合符合预期（C 还断言 `.h` 归属，
   C++ 断言不含 `h`）、query 发出的每个 kind 都能 `symbol_id_of` resolve；
2. `*_lang_cross_file_navigation`：`tempfile::tempdir` + `IndexBuilder::build` +
   `Navigator`，用小夹具断言若干 (符号, 文件, 行) 的定义/引用。

跨语言/注册表性质测试集中在 `tests/multilang_navigation.rs`（`mlang_` 前缀）。

### 步骤 6：跑验证命令

```bash
cargo build -p xai-codebase-graph
cargo test  -p xai-codebase-graph --quiet java_lang_
cargo test  -p xai-codebase-graph --quiet c_lang_
cargo test  -p xai-codebase-graph --quiet cpp_lang_
cargo test  -p xai-codebase-graph
```

### 三个工作示例的 query 摘录

`src/languages/java.rs`（方法定义；Java 没有外层字段的 import 用法见步骤 3）：

```
(method_declaration
    name: (identifier) @name.definition.method) @definition.method

(method_invocation
    name: (identifier) @name.reference.method) @reference.method
```

`src/languages/c.rs`（函数定义/原型 + 宏；`body: (_)` 守卫避免把
`struct Point *p` 这类 tag 用法当成定义）：

```
(function_declarator
    declarator: (identifier) @name.definition.function) @definition.function

(preproc_def
    name: (identifier) @name.definition.macro) @definition.macro

(struct_specifier
    name: (type_identifier) @name.definition.struct
    body: (_)) @definition.struct
```

`src/languages/cpp.rs`（namespace + out-of-line 方法 + `using` alias 映射）：

```
(namespace_definition
    name: (namespace_identifier) @name.definition.namespace) @definition.namespace

(function_declarator
    declarator: (qualified_identifier
        scope: (namespace_identifier)
        name: (identifier) @name.definition.method)) @definition.method

(alias_declaration
    name: (type_identifier) @alias.name
    type: (type_descriptor
        type: (type_identifier) @alias.original))
```

> 扩展一种语言时也应检查它会改变 `compute_query_hash`（新增配置 → hash 变），这是
> 期望行为：老缓存自动失效并重建，见 4.3。

---

## 8. CLI 与基准工具

### 8.1 `code-graph` CLI（`src/bin/code_graph.rs`，bin 名见 `Cargo.toml`）

基于 `clap` derive。四个子命令（缓存默认在 `<repo>/.goto_index.bin`，
可用 `--cache` 覆盖）：

```bash
# 构建/重建索引（--force 强制重建，--threads N 设线程）
code-graph index /path/to/repo
code-graph index /path/to/repo --cache /tmp/index.bin --force --threads 8

# 位置式 goto-definition（row/col 均为 1-indexed）
code-graph definition /path/to/repo --file src/main.rs --row 10 --col 15
# 按符号名直查
code-graph definition /path/to/repo --symbol MyStruct
# JSON 输出
code-graph definition /path/to/repo --symbol MyStruct --json

# goto-references（--include-definition 把定义也带出来）
code-graph references /path/to/repo --file src/main.rs --row 10 --col 15
code-graph references /path/to/repo --symbol MyStruct --include-definition

# 索引统计
code-graph stats /path/to/repo
```

内部行为（`load_or_build_index`）：先 `load_index(cache_path)`，缓存缺失/格式不符
（`CacheError::LegacyFormat`）时用 `IndexBuilder::new().build(repo)` 现建并落盘。
bin 里还挂 `mimalloc::MiMalloc` 全局分配器。

### 8.2 库内快速上手（`src/lib.rs` 顶部文档同款）

离线一次性构建 + 导航：

```rust,ignore
use std::path::Path;
use xai_codebase_graph::{IndexBuilder, load_index, save_index, get_cache_path, Navigator};

let repo_path = Path::new("/path/to/repo");
let cache_path = get_cache_path(repo_path);

let index = match load_index(&cache_path) {
    Ok(index) => index,
    Err(_) => {
        let index = IndexBuilder::new().with_threads(8).build(repo_path)?;
        save_index(&cache_path, &index)?;
        index
    }
};

let navigator = Navigator::new(index);
let result = navigator.goto_definition(Path::new("src/main.rs"), 10, 15)?;
```

常驻进程的增量更新 + 直查：

```rust,ignore
use std::path::PathBuf;
use xai_codebase_graph::{IndexManager, IndexManagerConfig, FileEvent};

let config = IndexManagerConfig::new("/path/to/repo".into())
    .with_cache_path("/tmp/index.bin".into());
let handle = IndexManager::spawn(config);

handle.send_event(FileEvent::modified("src/main.rs".into()))?;

let result = handle.goto_definition_blocking(PathBuf::from("src/main.rs"), 10, 15)??;
let file_count = handle.get_file_count();
let exists     = handle.has_definition_blocking("MyStruct");
```

### 8.3 基准 bin（`Cargo.toml` 的 `[[bin]]` 声明）

- **`bench_index`**（`src/bin/bench_index.rs`）：对给定仓库计时全量构建。启动时先
  “Verifying query compilation...”——对代码里硬编码的扩展列表
  `["ts","tsx","js","jsx","rs","go","py"]` 逐个 `config.compile_query()` 打印
  `OK (N patterns)` / `FAILED`（注意该列表早于 Java/C/C++ 加入，如需覆盖新语言要
  手动扩充），然后 `IndexBuilder::new().build(root)` 计时并打印
  文件数/定义数/引用数/别名数与耗时。仓库路径可用 `BENCH_REPO_ROOT`（或 `XAI_ROOT`）
  环境变量提供。
- **`bench_file_listing`**（`src/bin/bench_file_listing.rs`）：对比三种文件列表方式的
  耗时（参数 `<path> [cli|git2|git2-index|both]`）：(a) `cli` = 调 `git ls-files` +
  `git ls-files --others --exclude-standard`（跟踪 + 未跟踪）；(b) `git2` = `git2`
  index + `repo.statuses()` 补未跟踪；(c) `git2-index` = 只读 `git2` index 的跟踪文件
  （这正是 `IndexBuilder::collect_files_git` 采用的路线）。`both` 模式各跑 5 次求平均
  并打印 git2(index-only) 相对 CLI 的倍率。衡量这条收集路线的成本。

```bash
cargo run -p xai-codebase-graph --release --bin bench_index -- /path/to/repo
cargo run -p xai-codebase-graph --release --bin bench_file_listing -- /path/to/repo both
```

### 8.4 `code-graph-mcp`（stdio MCP server，`src/bin/code_graph_mcp.rs`）

把 code-graph 的查询能力暴露成 Model Context Protocol 工具：一个进程在
`<root>` 上构建（或加载缓存）索引后，通过 **stdin/stdout 换行分隔 JSON-RPC**
（MCP stdio transport）应答 `initialize` / `tools/list` / `tools/call` / `ping`。
**零新增依赖**（只用手写 JSON-RPC + 已有的 `serde_json`，不引 `rmcp`，从而不破坏
workspace 对 rmcp/reqwest 0.13 的隔离）。

```bash
# 构建并运行：启动时建索引（缓存默认 <root>/.goto_index.bin，可用 --cache 覆盖）
cargo run -p xai-codebase-graph --release --bin code-graph-mcp -- /path/to/repo

# 也可以先建好缓存再启动，缩短首次握手时间：
code-graph index /path/to/repo --cache ~/.cache/xcg/repo.bin
code-graph-mcp /path/to/repo --cache ~/.cache/xcg/repo.bin
```

暴露 6 个工具：`find_definitions(symbol, context_file?)`、
`find_references(symbol, context_file?, include_definition?)`、
`symbol_at_position(file, row, col)`、`goto_definition(file, row, col)`、
`goto_references(file, row, col, include_definition?)`、`stats()`。
`file`/`context_file` 相对 `<root>`（与索引存储一致），`row`/`col` 1-indexed，
结果与 CLI 一样是**行级**位置。缓存命中后会逐文件核对 size/mtime，任一文件过期即
重建（`index_is_fresh`，stale 语义见 4.2）。所有日志走 stderr，stdout 只输出协议。

把 server 注册给本地 MCP 宿主（Grok TUI 的 `~/.grok/config.toml` 或项目级
`.grok/config.toml`，等价配置对 Claude Code / Cursor 同样适用）：

```toml
[mcp_servers.code-graph]
command = "/absolute/path/to/code-graph-mcp"
args = ["/path/to/repo", "--cache", "/absolute/path/to/index.bin"]
enabled = true
```

Grok TUI 也可用命令注册后 `/mcps` 里开关：`grok mcp add code-graph -- /abs/path/code-graph-mcp /path/to/repo --cache /abs/path/index.bin`。

---

## 附：本文论断的“证据地图”

| 论断 | 去哪里核对 |
| --- | --- |
| 捕获名契约（`name.definition.*` / `alias.*` / 外层标记被忽略） | `src/scope_graph/graph.rs` `extract_symbols_fast`；`src/manager/builder.rs` `extract_symbols_fast_inline`；`src/index_manager.rs` `intern_symbols_directly` |
| 空 query 静默兜底 | `src/manager/builder.rs` `process_file_fast`、`src/index_manager.rs` `reindex_file` 的 `compile_query().unwrap_or_else(...)` |
| SGIX magic / version / query_version 序列化 | `src/scope_graph/graph.rs` `SCOPE_GRAPH_INDEX_MAGIC` / `SCOPE_GRAPH_INDEX_VERSION` / `write_to` / `read_from` |
| query hash 使缓存失效 | `src/languages/mod.rs` `compute_query_hash`；`src/scope_graph/graph.rs` `QueryVersion::needs_rebuild`；`src/index_manager.rs` `IndexManager::spawn` |
| StringInterner 结构与去重 | `src/interner.rs`（arena/lookup/offsets、`from_parts`） |
| definitions/references 的 `(StringId, u32)` 存储与反向索引 O(symbols_in_file) 删除 | `src/scope_graph/graph.rs`（字段定义、`remove_file`、`rename_file`、`add_definition_with_path_id`） |
| CoalescedEvents 合并语义 | `src/index_manager.rs` `CoalescedEvents::add/insert` 及同名单测 |
| 单写者 actor / COW / 轻量查询 | `src/index_manager.rs` `IndexManager::spawn`、`run_loop`、`process_command_coalesced`、`IndexManagerHandle` |
| 后台校验 + 跨进程锁 | `src/index_manager.rs` `background_index_refresh`；`src/manager/lock.rs` `try_lock` / `IndexOperation` |
| 位置解析与双份 `is_identifier_like` | `src/navigation.rs`、`src/index_manager.rs`（函数体带 keep-in-sync 注释）；证据测试 `evidence_name_capture_node_kinds_java_c_cpp` |
| 同语言优先排序 | `src/scope_graph/graph.rs` `find_definitions_smart` / `find_references_smart` / `extensions_same_language` |
| `.h` → C、单主归属不变量 | `src/languages/c.rs` / `cpp.rs`；`tests/multilang_navigation.rs` `mlang_registry_single_owner_per_extension_and_families` |
