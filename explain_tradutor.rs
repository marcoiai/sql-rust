//! Tradutor e Analisador de EXPLAIN SQL em Rust (CLI Híbrida: Texto + JSON)
//!
//! Como compilar e usar:
//!   rustc explain_tradutor.rs -O -o explain_tradutor
//!   ./explain_tradutor --demo
//!   psql -d meu_banco -c "EXPLAIN (ANALYZE, BUFFERS) SELECT ..." | ./explain_tradutor
//!   ./explain_tradutor plano.txt
//!
//! Destaques da arquitetura em Rust:
//! 1. Arena Pattern (`Vec<PlanNode>` com índices `usize` para pais/filhos):
//!    Evita completamente `Rc<RefCell<T>>` e conflitos com o Borrow Checker.
//! 2. Enum `NodeKind` + Pattern Matching exaustivo:
//!    Cada operador do banco (`SeqScan`, `IndexScan`, `IndexOnlyScan`, `Sort`, `NestedLoop`)
//!    é fortemente tipado para gerar diagnósticos e sugestões de índices.
//! 3. Dica para projetos Cargo (`FORMAT JSON`):
//!    No final do arquivo há o mapeamento pronto com `#[derive(Deserialize)]` do `serde`
//!    caso queira ler `EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON)` em um projeto Cargo.

use std::env;
use std::fs;
use std::io::{self, IsTerminal, Read};

#[derive(Debug, Clone, PartialEq)]
pub enum NodeKind {
    SeqScan { table: String },
    IndexScan { table: String, index: String },
    IndexOnlyScan { table: String, index: String },
    BitmapHeapScan { table: String },
    Sort,
    NestedLoop,
    HashJoin,
    Aggregate,
    Filter,
    Other(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Good,
    Info,
    Warn,
    Critical,
}

#[derive(Debug, Clone)]
pub struct Recommendation {
    pub severity: Severity,
    pub title: String,
    pub reason: String,
    pub suggested_sql: String,
}

#[derive(Debug, Clone)]
pub struct PlanNode {
    pub id: usize,
    pub indent: usize,
    pub raw_title: String,
    pub kind: NodeKind,
    pub cost_end: Option<f64>,
    pub est_rows: Option<u64>,
    pub actual_end_ms: Option<f64>,
    pub actual_rows: Option<u64>,
    pub loops: u64,
    pub filter: Option<String>,
    pub index_cond: Option<String>,
    pub sort_key: Option<String>,
    pub sort_method: Option<String>,
    pub rows_removed: u64,
    pub heap_fetches: Option<u64>,
    pub shared_hit: u64,
    pub shared_read: u64,
    pub temp_written: u64,
    // Relacionamentos por índice (Arena pattern: zero atrito com o Borrow Checker)
    pub parent_id: Option<usize>,
    pub children: Vec<usize>,
    // Métricas calculadas
    pub total_time_ms: Option<f64>,
    pub exclusive_time_ms: f64,
    pub share_pct: f64,
    pub exec_step_order: usize,
}

impl PlanNode {
    pub fn new(id: usize, indent: usize, raw_title: String, kind: NodeKind) -> Self {
        Self {
            id,
            indent,
            raw_title,
            kind,
            cost_end: None,
            est_rows: None,
            actual_end_ms: None,
            actual_rows: None,
            loops: 1,
            filter: None,
            index_cond: None,
            sort_key: None,
            sort_method: None,
            rows_removed: 0,
            heap_fetches: None,
            shared_hit: 0,
            shared_read: 0,
            temp_written: 0,
            parent_id: None,
            children: Vec::new(),
            total_time_ms: None,
            exclusive_time_ms: 0.0,
            share_pct: 0.0,
            exec_step_order: 0,
        }
    }
}

pub struct ExplainReport {
    pub engine: String,
    pub nodes: Vec<PlanNode>,
    pub planning_time_ms: Option<f64>,
    pub execution_time_ms: Option<f64>,
    pub recommendations: Vec<Recommendation>,
    pub health_score: u8,
}

// ============================================================================
// PARSER & CLASSIFICADOR DE OPERADORES
// ============================================================================

fn classify_operator(raw_title: &str) -> (NodeKind, Option<String>, Option<String>) {
    let lower = raw_title.to_lowercase();

    if lower.starts_with("index only scan using ") {
        let (idx, tbl) = extract_using_on(raw_title, "Index Only Scan using ");
        return (NodeKind::IndexOnlyScan { table: tbl, index: idx }, None, None);
    }
    if lower.starts_with("index scan using ") || lower.starts_with("bitmap index scan on ") {
        let (idx, tbl) = extract_using_on(raw_title, "Index Scan using ");
        return (NodeKind::IndexScan { table: tbl, index: idx }, None, None);
    }
    if lower.starts_with("seq scan on ") {
        let tbl = extract_word_after(raw_title, " on ");
        return (NodeKind::SeqScan { table: tbl }, None, None);
    }
    if lower.starts_with("bitmap heap scan on ") {
        let tbl = extract_word_after(raw_title, " on ");
        return (NodeKind::BitmapHeapScan { table: tbl }, None, None);
    }
    if lower.starts_with("table scan on ") {
        let tbl = extract_word_after(raw_title, " on ");
        if tbl.contains("temporary") {
            return (NodeKind::Aggregate, None, None);
        }
        return (NodeKind::SeqScan { table: tbl }, None, None);
    }
    if lower.starts_with("index lookup on ") {
        let tbl = extract_word_after(raw_title, " on ");
        let idx = extract_word_after(raw_title, " using ");
        return (NodeKind::IndexScan { table: tbl, index: idx }, None, None);
    }
    if lower.starts_with("filter:") {
        let cond = raw_title.get(7..).unwrap_or("").trim().to_string();
        return (NodeKind::Filter, Some(cond), None);
    }
    if lower.starts_with("sort:") {
        let key = raw_title.get(5..).unwrap_or("").trim().to_string();
        return (NodeKind::Sort, None, Some(key));
    }
    if lower.starts_with("sort") {
        return (NodeKind::Sort, None, None);
    }
    if lower.contains("nested loop") {
        return (NodeKind::NestedLoop, None, None);
    }
    if lower.contains("hash join") {
        return (NodeKind::HashJoin, None, None);
    }
    if lower.contains("aggregate") || lower.contains("group") {
        return (NodeKind::Aggregate, None, None);
    }

    (NodeKind::Other(raw_title.to_string()), None, None)
}

fn extract_using_on(s: &str, prefix: &str) -> (String, String) {
    let rest = if s.len() >= prefix.len() { &s[prefix.len()..] } else { s };
    if let Some(on_pos) = rest.find(" on ") {
        let idx = rest[..on_pos].trim().to_string();
        let after_on = rest[on_pos + 4..].trim();
        let tbl = after_on.split_whitespace().next().unwrap_or("tabela").to_string();
        (idx, tbl)
    } else {
        ("indice".to_string(), "tabela".to_string())
    }
}

fn extract_word_after(s: &str, needle: &str) -> String {
    if let Some(pos) = s.to_lowercase().find(&needle.to_lowercase()) {
        let after = s[pos + needle.len()..].trim();
        after.split_whitespace().next().unwrap_or("tabela").to_string()
    } else {
        "tabela".to_string()
    }
}

fn extract_number_after(line: &str, key: &str) -> Option<f64> {
    let pos = line.find(key)?;
    let rest = &line[pos + key.len()..];
    let num_str: String = rest
        .chars()
        .skip_while(|c| c.is_whitespace())
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    num_str.parse::<f64>().ok()
}

fn parse_metrics_blocks(header: &str, node: &mut PlanNode) {
    let mut start_idx = 0;
    while let Some(open) = header[start_idx..].find('(') {
        let abs_open = start_idx + open;
        if let Some(close) = header[abs_open..].find(')') {
            let abs_close = abs_open + close;
            let inside = &header[abs_open + 1..abs_close];

            if inside.contains("cost=") {
                if let Some(dots) = inside.find("..") {
                    let after_dots = &inside[dots + 2..];
                    let cost_str: String = after_dots
                        .chars()
                        .take_while(|c| c.is_ascii_digit() || *c == '.')
                        .collect();
                    node.cost_end = cost_str.parse::<f64>().ok();
                } else {
                    node.cost_end = extract_number_after(inside, "cost=");
                }
                if let Some(r) = extract_number_after(inside, "rows=") {
                    node.est_rows = Some(r as u64);
                }
            } else if inside.contains("actual time=") {
                if let Some(dots) = inside.find("..") {
                    let after_dots = &inside[dots + 2..];
                    let time_str: String = after_dots
                        .chars()
                        .take_while(|c| c.is_ascii_digit() || *c == '.')
                        .collect();
                    node.actual_end_ms = time_str.parse::<f64>().ok();
                }
                if let Some(r) = extract_number_after(inside, "rows=") {
                    node.actual_rows = Some(r as u64);
                }
                if let Some(l) = extract_number_after(inside, "loops=") {
                    node.loops = (l as u64).max(1);
                }
            }
            start_idx = abs_close + 1;
        } else {
            break;
        }
    }

    if let Some(end_ms) = node.actual_end_ms {
        node.total_time_ms = Some(end_ms * (node.loops as f64));
    }
}

pub fn analyze_explain(raw_text: &str) -> ExplainReport {
    let mut engine = "PostgreSQL".to_string();
    if raw_text.contains("Table scan on") || raw_text.contains("Index lookup on") {
        engine = "MySQL 8+".to_string();
    }

    let mut nodes: Vec<PlanNode> = Vec::new();
    let mut planning_time_ms = None;
    let mut execution_time_ms = None;

    for line in raw_text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if trimmed.starts_with("Planning Time:") {
            planning_time_ms = extract_number_after(trimmed, "Planning Time:");
            continue;
        }
        if trimmed.starts_with("Execution Time:") {
            execution_time_ms = extract_number_after(trimmed, "Execution Time:");
            continue;
        }

        let arrow_pos = line.find("->");
        let has_metrics = line.contains("(cost=") || line.contains("(actual time=");

        if arrow_pos.is_some() || (nodes.is_empty() && has_metrics) {
            let indent = arrow_pos.unwrap_or_else(|| line.chars().take_while(|c| c.is_whitespace()).count());
            let header = match arrow_pos {
                Some(pos) => line[pos + 2..].trim(),
                None => trimmed,
            };

            let raw_title = match header.find('(') {
                Some(p) => header[..p].trim().to_string(),
                None => header.to_string(),
            };

            let (kind, inline_filter, inline_sort) = classify_operator(&raw_title);
            let mut node = PlanNode::new(nodes.len(), indent, raw_title, kind);
            node.filter = inline_filter;
            node.sort_key = inline_sort;
            parse_metrics_blocks(header, &mut node);
            nodes.push(node);
        } else if let Some(current) = nodes.last_mut() {
            if let Some(rest) = trimmed.strip_prefix("Filter:") {
                current.filter = Some(rest.trim().to_string());
            } else if let Some(rest) = trimmed.strip_prefix("Index Cond:") {
                current.index_cond = Some(rest.trim().to_string());
            } else if let Some(rest) = trimmed.strip_prefix("Sort Key:") {
                current.sort_key = Some(rest.trim().to_string());
            } else if let Some(rest) = trimmed.strip_prefix("Sort Method:") {
                current.sort_method = Some(rest.trim().to_string());
            } else if trimmed.starts_with("Rows Removed by ") {
                if let Some(n) = extract_number_after(trimmed, ":") {
                    current.rows_removed += n as u64;
                }
            } else if trimmed.starts_with("Heap Fetches:") {
                if let Some(n) = extract_number_after(trimmed, "Heap Fetches:") {
                    current.heap_fetches = Some(n as u64);
                }
            } else if trimmed.starts_with("Buffers:") {
                if let Some(h) = extract_number_after(trimmed, "shared hit=") {
                    current.shared_hit += h as u64;
                }
                if let Some(r) = extract_number_after(trimmed, "shared read=") {
                    current.shared_read += r as u64;
                }
                if let Some(w) = extract_number_after(trimmed, "written=") {
                    current.temp_written += w as u64;
                }
            }
        }
    }

    // 1. Constrói a árvore usando pilha de indentação (Arena flat sem referências cíclicas)
    let mut stack: Vec<(usize, usize)> = Vec::new(); // (node_id, indent)
    for i in 0..nodes.len() {
        let indent = nodes[i].indent;
        while let Some(&(_, top_indent)) = stack.last() {
            if top_indent >= indent {
                stack.pop();
            } else {
                break;
            }
        }
        if let Some(&(parent_id, _)) = stack.last() {
            nodes[i].parent_id = Some(parent_id);
            nodes[parent_id].children.push(i);
        }
        stack.push((i, indent));
    }

    // 2. Calcula o Tempo Exclusivo de cada nó (descontando os nós filhos)
    let root_total_ms = execution_time_ms
        .or_else(|| nodes.first().and_then(|n| n.total_time_ms))
        .unwrap_or(0.0);

    for i in 0..nodes.len() {
        let total_ms = nodes[i].total_time_ms.unwrap_or(0.0);
        let children_sum: f64 = nodes[i]
            .children
            .iter()
            .filter_map(|&cid| nodes[cid].total_time_ms)
            .sum();
        let exclusive = (total_ms - children_sum).max(0.0);
        nodes[i].exclusive_time_ms = exclusive;
        nodes[i].share_pct = if root_total_ms > 0.0 {
            ((exclusive / root_total_ms) * 100.0).min(100.0)
        } else {
            0.0
        };
    }

    // 3. Numera a Ordem Real de Execução (Travessia Pós-Ordem: das folhas até a raiz)
    let mut exec_order = Vec::new();
    for i in 0..nodes.len() {
        if nodes[i].parent_id.is_none() {
            post_order_traversal(i, &nodes, &mut exec_order);
        }
    }
    for (step_idx, &node_id) in exec_order.iter().enumerate() {
        nodes[node_id].exec_step_order = step_idx + 1;
    }

    // 4. Diagnóstico com Pattern Matching e geração de SQL recomendado
    let mut recommendations = Vec::new();
    let mut score: i32 = 100;

    for node in &nodes {
        let removed_total = node.rows_removed * node.loops;
        match &node.kind {
            NodeKind::SeqScan { table } => {
                if removed_total > 500 || node.actual_rows.unwrap_or(0) * node.loops > 5000 {
                    score -= 30;
                    let filter_str = node.filter.as_deref().unwrap_or("colunas_do_where");
                    recommendations.push(Recommendation {
                        severity: Severity::Critical,
                        title: format!("Evitar Seq Scan na tabela '{}'", table),
                        reason: format!(
                            "O {}º passo varreu a tabela '{}' inteira (repetido {}x) e descartou {} linhas no filtro {}.",
                            node.exec_step_order, table, node.loops, removed_total, filter_str
                        ),
                        suggested_sql: format!(
                            "CREATE INDEX idx_{}_filtro ON {} (/* colunas de {} */);",
                            table, table, filter_str
                        ),
                    });
                }
            }
            NodeKind::Sort => {
                let is_disk = node
                    .sort_method
                    .as_deref()
                    .map(|m| m.to_lowercase().contains("disk"))
                    .unwrap_or(false)
                    || node.temp_written > 0;
                if is_disk {
                    score -= 25;
                    let key = node.sort_key.as_deref().unwrap_or("coluna_order_by");
                    recommendations.push(Recommendation {
                        severity: Severity::Critical,
                        title: format!("Eliminar ordenação em disco ({})", key),
                        reason: "A ordenação ultrapassou o limite de RAM (work_mem) e precisou gravar arquivos temporários em disco.".to_string(),
                        suggested_sql: format!(
                            "CREATE INDEX idx_ordenacao ON sua_tabela (colunas_where, {}); -- ou SET work_mem = '64MB';",
                            key
                        ),
                    });
                }
            }
            NodeKind::Filter => {
                if let Some(f) = &node.filter {
                    let upper = f.to_uppercase();
                    if upper.contains("YEAR(") || upper.contains("MONTH(") || upper.contains("LOWER(") {
                        score -= 25;
                        recommendations.push(Recommendation {
                            severity: Severity::Critical,
                            title: format!("Remover função sobre coluna filtrada: {}", f),
                            reason: "Funções aplicadas diretamente na coluna impedem o uso de índices B-Tree.".to_string(),
                            suggested_sql: "WHERE data_criacao >= '2026-01-01' AND data_criacao < '2027-01-01'".to_string(),
                        });
                    }
                }
            }
            _ => {}
        }

        // Checa erro grosseiro de estimativa de linhas (Estatísticas defasadas)
        if let (Some(est), Some(act)) = (node.est_rows, node.actual_rows) {
            if est > 0 && act > 200 && (act / est >= 10 || est / act.max(1) >= 10) {
                score -= 15;
                recommendations.push(Recommendation {
                    severity: Severity::Warn,
                    title: format!("Estatísticas defasadas no {}º passo ({})", node.exec_step_order, node.raw_title),
                    reason: format!(
                        "O planejador estimou {} linhas, mas vieram {} linhas reais por loop.",
                        est, act
                    ),
                    suggested_sql: "ANALYZE sua_tabela;".to_string(),
                });
            }
        }
    }

    ExplainReport {
        engine,
        nodes,
        planning_time_ms,
        execution_time_ms: Some(root_total_ms),
        recommendations,
        health_score: score.clamp(10, 100) as u8,
    }
}

fn post_order_traversal(node_id: usize, nodes: &[PlanNode], out: &mut Vec<usize>) {
    for &child_id in &nodes[node_id].children {
        post_order_traversal(child_id, nodes, out);
    }
    out.push(node_id);
}

// ============================================================================
// FORMATAÇÃO HUMANA NO TERMINAL (COM CORES ANSI)
// ============================================================================

pub fn print_human_report(report: &ExplainReport) {
    let score_color = if report.health_score >= 80 {
        "\x1b[1;32m" // Verde
    } else if report.health_score >= 50 {
        "\x1b[1;33m" // Amarelo
    } else {
        "\x1b[1;31m" // Vermelho
    };

    println!("\x1b[1;36m========================================================================\x1b[0m");
    println!(
        "\x1b[1;37m TRADUTOR DE EXPLAIN SQL (Rust CLI)\x1b[0m | Motor: \x1b[1;34m{}\x1b[0m | Saúde: {}{}/100\x1b[0m",
        report.engine, score_color, report.health_score
    );
    if let Some(exec_ms) = report.execution_time_ms {
        print!(" Tempo Total Real: \x1b[1;37m{:.2} ms\x1b[0m", exec_ms);
        if let Some(plan_ms) = report.planning_time_ms {
            print!(" (Planejamento: {:.2} ms)", plan_ms);
        }
        println!();
    }
    println!("\x1b[1;36m========================================================================\x1b[0m\n");

    println!("\x1b[1;37m1. PASSO A PASSO NA ORDEM REAL DE EXECUÇÃO (De dentro para fora):\x1b[0m");
    let mut ordered: Vec<&PlanNode> = report.nodes.iter().collect();
    ordered.sort_by_key(|n| n.exec_step_order);

    for node in ordered {
        let pct = node.share_pct.round() as usize;
        let bar_len = (pct / 5).max(1);
        let bar = "█".repeat(bar_len);
        let color = if node.share_pct >= 50.0 {
            "\x1b[31m"
        } else if node.share_pct >= 20.0 {
            "\x1b[33m"
        } else {
            "\x1b[32m"
        };

        let human_desc = match &node.kind {
            NodeKind::SeqScan { table } => format!(
                "Leu a tabela '{}' inteira sem índice (Seq Scan){}",
                table,
                node.filter
                    .as_ref()
                    .map(|f| format!(", filtrando {}", f))
                    .unwrap_or_default()
            ),
            NodeKind::IndexScan { table, index } => {
                format!("Buscou na tabela '{}' usando o índice '{}'", table, index)
            }
            NodeKind::IndexOnlyScan { table, index } => {
                format!("Busca 100% coberta pelo índice '{}' na tabela '{}'", index, table)
            }
            NodeKind::BitmapHeapScan { table } => {
                format!("Leitura em lote de páginas (Bitmap Heap Scan) na tabela '{}'", table)
            }
            NodeKind::Sort => format!(
                "Ordenou os resultados por {} ({})",
                node.sort_key.as_deref().unwrap_or("ORDER BY"),
                node.sort_method.as_deref().unwrap_or("em memória")
            ),
            NodeKind::NestedLoop => "Cruzou as tabelas em laço (Nested Loop Join)".to_string(),
            NodeKind::HashJoin => "Cruzou as tabelas usando Hash Join em memória".to_string(),
            NodeKind::Aggregate => "Agrupou resultados (GROUP BY / Tabela Temporária)".to_string(),
            NodeKind::Filter => format!(
                "Filtrou linhas com a condição {}",
                node.filter.as_deref().unwrap_or("")
            ),
            NodeKind::Other(raw) => format!("Executou: {}", raw),
        };

        let rows_total = node.actual_rows.unwrap_or(node.est_rows.unwrap_or(0)) * node.loops;
        println!(
            "  \x1b[1;36m[{}º Passo]\x1b[0m {} {:>6.2} ms ({:>3.0}%) {}{}\x1b[0m",
            node.exec_step_order,
            human_desc,
            node.exclusive_time_ms,
            node.share_pct,
            color,
            bar
        );
        println!(
            "             └─ Linhas entregues: {} (loops={}x, descartadas no filtro={})",
            rows_total,
            node.loops,
            node.rows_removed * node.loops
        );
    }

    println!("\n\x1b[1;37m2. DIAGNÓSTICO E ÍNDICES SUGERIDOS:\x1b[0m");
    if report.recommendations.is_empty() {
        println!("  \x1b[1;32m✔ Nenhum gargalo crítico detectado! O plano já usa índices de forma eficiente.\x1b[0m");
    } else {
        for (i, rec) in report.recommendations.iter().enumerate() {
            let tag = match rec.severity {
                Severity::Critical => "\x1b[1;31m[ALTO IMPACTO]\x1b[0m",
                Severity::Warn => "\x1b[1;33m[ATENÇÃO]\x1b[0m",
                _ => "\x1b[1;34m[DICA]\x1b[0m",
            };
            println!("  {}. {} \x1b[1m{}\x1b[0m", i + 1, tag, rec.title);
            println!("     • Motivo: {}", rec.reason);
            println!("     • SQL Sugerido: \x1b[1;32m{}\x1b[0m\n", rec.suggested_sql);
        }
    }
}

const DEMO_PLAN: &str = r#"Sort  (cost=14520.45..14524.30 rows=1540 width=128) (actual time=184.320..186.910 rows=1550 loops=1)
  Sort Key: p.data_pedido DESC
  Sort Method: external merge  Disk: 4120kB
  Buffers: shared hit=1240 read=8420, temp read=515 written=515
  ->  Seq Scan on pedidos p  (cost=0.00..12110.00 rows=1540 width=128) (actual time=0.045..142.610 rows=1550 loops=1)
        Filter: ((status = 'PAGO'::text) AND (valor_total > 500.00))
        Rows Removed by Filter: 198450
        Buffers: shared hit=1240 read=8420
Planning Time: 0.840 ms
Execution Time: 189.450 ms"#;

fn main() {
    let args: Vec<String> = env::args().collect();

    let input = if args.len() > 1 && args[1] != "--demo" {
        fs::read_to_string(&args[1]).unwrap_or_else(|e| {
            eprintln!("Erro ao ler arquivo '{}': {}", args[1], e);
            std::process::exit(1);
        })
    } else if !io::stdin().is_terminal() && (args.len() == 1) {
        let mut buf = String::new();
        io::stdin().read_to_string(&mut buf).unwrap_or_default();
        if buf.trim().is_empty() {
            DEMO_PLAN.to_string()
        } else {
            buf
        }
    } else {
        DEMO_PLAN.to_string()
    };

    let report = analyze_explain(&input);
    print_human_report(&report);
}

// ============================================================================
// BÔNUS: MAPEAMENTO SERDE PARA `EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON)`
// ============================================================================
// Se você criar um projeto com `cargo new explain-rs` e adicionar:
//   cargo add serde --features derive
//   cargo add serde_json
// basta usar estas structs para ler o JSON oficial do PostgreSQL sem parser manual:
//
// #[derive(serde::Deserialize, Debug)]
// pub struct PgExplainJsonRoot {
//     #[serde(rename = "Plan")]
//     pub plan: PgJsonPlanNode,
//     #[serde(rename = "Planning Time")]
//     pub planning_time: Option<f64>,
//     #[serde(rename = "Execution Time")]
//     pub execution_time: Option<f64>,
// }
//
// #[derive(serde::Deserialize, Debug)]
// pub struct PgJsonPlanNode {
//     #[serde(rename = "Node Type")]
//     pub node_type: String,
//     #[serde(rename = "Relation Name")]
//     pub relation_name: Option<String>,
//     #[serde(rename = "Index Name")]
//     pub index_name: Option<String>,
//     #[serde(rename = "Total Cost")]
//     pub total_cost: Option<f64>,
//     #[serde(rename = "Plan Rows")]
//     pub plan_rows: Option<u64>,
//     #[serde(rename = "Actual Total Time")]
//     pub actual_total_time: Option<f64>,
//     #[serde(rename = "Actual Rows")]
//     pub actual_rows: Option<u64>,
//     #[serde(rename = "Actual Loops")]
//     pub actual_loops: Option<u64>,
//     #[serde(rename = "Filter")]
//     pub filter: Option<String>,
//     #[serde(rename = "Rows Removed by Filter")]
//     pub rows_removed_by_filter: Option<u64>,
//     #[serde(rename = "Plans", default)]
//     pub plans: Vec<PgJsonPlanNode>,
// }