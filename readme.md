# Tradutor e Analisador de EXPLAIN SQL (Web & Rust CLI)

Utilitário para traduzir planos de execução de bancos de dados (`EXPLAIN` e `EXPLAIN ANALYZE` do **PostgreSQL** e **MySQL 8+**) em explicações claras em português, ordenadas na sequência real de execução e acompanhadas de diagnósticos e sugestões de índices.

## Arquivos do Projeto

* **`explain_tradutor.html`**: Aplicativo web interativo (arquivo único autocontido) com painel de indicadores, ordem real de execução, gerador de índices e dicionário de termos.
* **`explain_tradutor.rs`**: Ferramenta de linha de comando (CLI) em Rust puro, sem dependências externas obrigatórias, feita para receber planos direto via pipe (`|`) do `psql` ou de arquivos.

## Como Ler um `EXPLAIN` na Prática

### 1. Gere o plano com métricas reais (`ANALYZE`)
O comando `EXPLAIN` sozinho exibe apenas estimativas baseadas nas estatísticas do banco. Para descobrir o gargalo real, execute a query medindo o tempo e os blocos lidos:

* **No PostgreSQL:**
  ```sql
  EXPLAIN (ANALYZE, BUFFERS)
  SELECT * FROM pedidos WHERE status = 'PAGO';
  ```
* **No MySQL (8.0.18+):**
  ```sql
  EXPLAIN ANALYZE
  SELECT * FROM pedidos WHERE status = 'PAGO';
  ```
* **Cuidado com `UPDATE`, `DELETE` e `INSERT`:** como o `EXPLAIN ANALYZE` executa a query de verdade, envolva comandos de escrita em uma transação com `ROLLBACK`:
  ```sql
  BEGIN;
  EXPLAIN (ANALYZE, BUFFERS) DELETE FROM pedidos WHERE status = 'CANCELADO';
  ROLLBACK;
  ```

### 2. Três Regras de Ouro da Leitura
1. **Leia de dentro para fora (ou de baixo para cima):** o nó mais indentado (com mais setas `->`) executa primeiro e entrega as linhas para o nó pai acima dele.
2. **Multiplique `actual time` por `loops`:** no PostgreSQL, o `actual time` mostra a média por execução daquele nó. Se um nó leva `0.15 ms` mas tem `loops=4500`, ele custou `675 ms` no total.
3. **Olhe para o Tempo Exclusivo:** o nó raiz sempre mostra ~100% do tempo total porque soma o tempo de todos os filhos. O verdadeiro gargalo é o nó com maior **tempo exclusivo** (`tempo total do nó - soma dos filhos`).

### 3. Tabela Rápida de Gargalos e Soluções

| Sinal no EXPLAIN (PostgreSQL / MySQL) | O que significa | Como otimizar |
| :--- | :--- | :--- |
| **Seq Scan** (PG) / **Table scan / ALL** (MySQL) | O banco leu a tabela inteira do início ao fim. | Crie um índice nas colunas usadas no `WHERE` ou `JOIN` (quando o filtro é seletivo). |
| **Rows Removed by Filter** alto (PG) / **filtered** baixo (MySQL) | O banco leu muitas linhas do disco apenas para descartá-las em seguida. | Crie um **índice composto** cobrindo todas as colunas do filtro. |
| **Sort Method: external merge Disk** (PG) / **Using filesort** (MySQL) | A ordenação não coube na memória RAM (`work_mem`) e precisou gravar arquivos temporários em disco. | Crie um índice com a mesma ordem do `WHERE` + `ORDER BY` ou aumente o `work_mem`. |
| **HashAggregate / GroupAggregate** pesado / **Using temporary** | O banco criou uma tabela temporária para resolver um `GROUP BY` ou `DISTINCT`. | Filtre mais linhas antes de agrupar ou indexe as colunas do agrupamento. |
| **Index Scan** em vez de **Index Only Scan** | O banco achou o registro no índice, mas precisou ir até a tabela buscar outras colunas. | Evite `SELECT *` ou use `CREATE INDEX ... INCLUDE (colunas)` para cobrir a consulta. |
| **Diferença grande entre `rows` estimado e `actual rows`** | O planejador escolheu um plano ruim (ex: `Nested Loop` em vez de `Hash Join`) porque as estatísticas estão defasadas. | Rode `ANALYZE nome_da_tabela;` para atualizar as estatísticas. |

### 4. Armadilhas no SQL que Desativam Índices
* **Funções aplicadas na coluna filtrada:** escrever `WHERE YEAR(data_criacao) = 2026` impede o uso de índices B-Tree. Prefira intervalos diretos: `WHERE data_criacao >= '2026-01-01' AND data_criacao < '2027-01-01'`.
* **Curinga no início do `LIKE`:** `WHERE nome LIKE '%silva%'` não usa índice B-Tree comum (apenas prefixos como `'silva%'`). Para buscas no meio do texto, utilize Full-Text Search ou `pg_trgm` (GIN/GiST).
* **Conversão implícita de tipos:** comparar uma coluna `VARCHAR` com número (`WHERE cpf = 12345678900`) obriga o banco a converter todas as linhas da tabela.

## Como Compilar e Usar a CLI em Rust

### Compilação
O arquivo `explain_tradutor.rs` usa apenas a biblioteca padrão do Rust (`std`), compilando direto com `rustc`:

```bash
rustc explain_tradutor.rs -O -o explain_tradutor
```

### Formas de Execução Básica
* **Rodar com o plano de demonstração embutido:**
  ```bash
  ./explain_tradutor --demo
  ```
* **Ler um plano salvo em arquivo de texto:**
  ```bash
  ./explain_tradutor meu_explain.txt
  ```

## Conectando Direto ao `psql` (Sem Digitar Senha e com Queries Gigantes)

### 1. Como passar a senha direto para o `psql`
* **Via variável de ambiente `PGPASSWORD` (mais prático para uso pontual):**
  ```bash
  PGPASSWORD='sua_senha' psql -h localhost -U seu_usuario -d seu_banco -qtA -c "EXPLAIN (ANALYZE, BUFFERS) SELECT ..." | ./explain_tradutor
  ```
* **Via Connection URI completa:**
  ```bash
  psql "postgresql://seu_usuario:sua_senha@localhost:5432/seu_banco" -qtA -c "EXPLAIN (ANALYZE, BUFFERS) SELECT ..." | ./explain_tradutor
  ```
* **Via arquivo `~/.pgpass` (mais seguro para não expor a senha no histórico do bash):**
  ```bash
  echo "localhost:5432:seu_banco:seu_usuario:sua_senha" >> ~/.pgpass
  chmod 600 ~/.pgpass
  psql -h localhost -U seu_usuario -d seu_banco -qtA -c "EXPLAIN (ANALYZE, BUFFERS) SELECT ..." | ./explain_tradutor
  ```
*(As flags `-qtA` removem cabeçalhos, bordas e contagem de linhas da saída do `psql`).*

### 2. Como passar arquivos `.sql` com queries gigantes
* **Se o arquivo `.sql` já começa com `EXPLAIN (ANALYZE, BUFFERS)`:**
  ```bash
  PGPASSWORD='sua_senha' psql -h localhost -U seu_usuario -d seu_banco -qtA -f query_monstruosa.sql | ./explain_tradutor
  ```
* **Se o arquivo `.sql` contém apenas a query pura (injeta o `EXPLAIN` automaticamente sem editar o arquivo):**
  ```bash
  { echo "EXPLAIN (ANALYZE, BUFFERS)"; cat query_monstruosa.sql; } | PGPASSWORD='sua_senha' psql -h localhost -U seu_usuario -d seu_banco -qtA | ./explain_tradutor
  ```
* **Se a query gigante faz alterações (`UPDATE`/`DELETE`/`INSERT`) e você quer proteger com `ROLLBACK`:**
  ```bash
  { echo "BEGIN; EXPLAIN (ANALYZE, BUFFERS)"; cat query_monstruosa.sql; echo "; ROLLBACK;"; } | PGPASSWORD='sua_senha' psql -h localhost -U seu_usuario -d seu_banco -qtA | ./explain_tradutor
  ```
* **Se a query no arquivo `.sql` usa variáveis do `psql` (ex: `:empresa_id` e `:data_inicio`):**
  ```bash
  { echo "EXPLAIN (ANALYZE, BUFFERS)"; cat query_monstruosa.sql; } | PGPASSWORD='sua_senha' psql -h localhost -U seu_usuario -d seu_banco -qtA -v empresa_id=42 -v data_inicio="'2026-01-01'" | ./explain_tradutor
  ```

## Arquitetura do Código em Rust

| Decisão de Design | Como foi implementada em `explain_tradutor.rs` | Benefício prático |
| :--- | :--- | :--- |
| **Arena Flat (`Vec<PlanNode>`)** | Pais e filhos guardam apenas índices `usize` (`parent_id: Option<usize>`, `children: Vec<usize>`). | Elimina lifetimes complexos (`'a`) e `Rc<RefCell<T>>` no Borrow Checker. |
| **Pilha de Indentação** | Lê a posição de `->` em cada linha e desempilha nós de indentação maior ou igual. | Reconstrói a hierarquia da árvore em uma única passagem $O(N)$. |
| **Travessia Pós-Ordem** | Percorre recursivamente as folhas antes dos pais (`post_order_traversal`). | Numera os passos na sequência exata em que o banco executa cada operação. |
| **Pattern Matching (`NodeKind`)** | Classifica cada operador em um `enum` fortemente tipado (`SeqScan`, `IndexScan`, `Sort`, etc.). | Garante tratamento estruturado de alertas e geração de comandos `CREATE INDEX`. |
