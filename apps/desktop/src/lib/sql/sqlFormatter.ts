import { DEFAULT_SQL_FORMATTER_SETTINGS, sqlFormatterOptions, type SqlFormatterSettings } from "@/lib/sql/sqlFormatterConfig";

export type SqlFormatDialect = "mysql" | "postgres" | "sqlite" | "sqlserver" | "clickhouse" | "generic";

export const MAX_SQL_FORMAT_CHARS = 1_000_000;

/**
 * Maps a connection's database type to the SQL-formatter dialect to use.
 *
 * Postgres-compatible engines (GaussDB/openGauss/Kingbase/...) reuse the
 * "postgres" grammar, SQLite-compatible ones reuse "sqlite", and anything
 * unrecognized falls back to the permissive "generic" dialect. Centralized
 * here so every surface that formats SQL (editor, object source, DDL viewers)
 * stays in sync.
 */
export function sqlFormatDialectForDbType(dbType: string | null | undefined): SqlFormatDialect {
  switch (dbType) {
    case "mysql":
      return "mysql";
    case "postgres":
    case "kwdb":
    case "gaussdb":
    case "opengauss":
    case "questdb":
    case "kingbase":
    case "highgo":
    case "vastbase":
    case "redshift":
      return "postgres";
    case "sqlite":
    case "rqlite":
    case "turso":
    case "cloudflare-d1":
      return "sqlite";
    case "sqlserver":
      return "sqlserver";
    case "clickhouse":
      return "clickhouse";
    default:
      return "generic";
  }
}

function formatterLanguage(dialect: SqlFormatDialect) {
  switch (dialect) {
    case "mysql":
      return "mysql";
    case "postgres":
      return "postgresql";
    case "sqlite":
      return "sqlite";
    case "sqlserver":
      return "transactsql";
    case "clickhouse":
      return "clickhouse";
    default:
      return "sql";
  }
}

export async function formatSqlText(sql: string, dialect: SqlFormatDialect = "generic", settings: Partial<SqlFormatterSettings> = DEFAULT_SQL_FORMATTER_SETTINGS): Promise<string> {
  if (!sql.trim()) return sql;
  if (sql.length > MAX_SQL_FORMAT_CHARS) {
    throw new Error("SQL is too large to format safely.");
  }

  const { format } = await import("sql-formatter");
  const options = sqlFormatterOptions(settings);
  const language = formatterLanguage(dialect);
  try {
    return format(sql, { language, ...options });
  } catch (err) {
    // The generic "sql" dialect can't parse many real-world constructs (PostgreSQL
    // `::` casts, GaussDB/openGauss materialized-view DDL, T-SQL specifics, ...).
    // Retry once with the more permissive PostgreSQL grammar, which is a superset
    // that tolerates most of these, before surfacing the failure.
    if (language !== "postgresql") {
      try {
        return format(sql, { language: "postgresql", ...options });
      } catch {
        // fall through to the original error below
      }
    }
    throw err;
  }
}

/**
 * 压缩 SQL 时使用的方言。不同方言对引号、注释、转义的处理不同：
 * - `mysql`：保留 MySQL 可执行注释与 optimizer hint；单引号字符串支持反斜杠转义
 * - `postgres`：支持 dollar-quoted 字符串
 * - `sqlserver`：支持方括号标识符
 * - `generic` / 其它：仅处理标准单/双引号与块/行注释
 */
export type SqlCompressDialect = SqlFormatDialect;

const DOLLAR_QUOTE_RE = /\$[A-Za-z_][A-Za-z0-9_]*\$|\$\$/;

/**
 * 将 SQL 压缩成一行可执行文本：折叠所有空白（含换行）为单个空格，
 * 移除普通行注释（-- ...）与普通块注释（/* ... *\/），
 * 同时按方言完整保留字符串字面量、引号标识符、可执行注释与 optimizer hint。
 *
 * 方言感知说明：
 * - MySQL：可执行注释作为可执行代码原样保留（仅折叠内部空白）；
 *   optimizer hint 原样保留；单引号字符串内反斜杠转义保留
 * - PostgreSQL：dollar-quoted 字符串原样保留（含标签形式）
 * - SQL Server：方括号标识符原样保留（双右括号为转义）
 * - 所有方言：单引号字符串、双引号标识符、反引号标识符均保留
 */
export function compressSqlText(sql: string, dialect: SqlCompressDialect = "generic"): string {
  if (!sql.trim()) return sql;

  const len = sql.length;
  let out = "";
  let i = 0;

  const isWhitespace = (c: string) => c === " " || c === "\t" || c === "\n" || c === "\r" || c === "\f" || c === "\v";

  // 折叠一段空白为单个空格（仅在 out 非空且不以空格结尾时追加）
  const collapseWhitespace = () => {
    while (i < len && isWhitespace(sql[i])) i++;
    if (out && !out.endsWith(" ")) out += " ";
  };

  while (i < len) {
    const ch = sql[i];
    const next = sql[i + 1];

    // 块注释 /* ... */ —— 需区分普通块注释、MySQL 可执行注释 /*! */、optimizer hint /*+ */
    if (ch === "/" && next === "*") {
      const third = sql[i + 2];
      const isExecutableMysql = third === "!";
      const isOptimizerHint = third === "+";

      if (isExecutableMysql || isOptimizerHint) {
        // 可执行注释 / optimizer hint —— 保留整体（含界定符），仅折叠内部空白
        out += "/*";
        i += 2;
        if (isExecutableMysql) {
          out += "!";
          i++;
        } else {
          out += "+";
          i++;
        }
        while (i < len) {
          if (sql[i] === "*" && sql[i + 1] === "/") {
            out += "*/";
            i += 2;
            break;
          }
          if (isWhitespace(sql[i])) {
            collapseWhitespace();
          } else {
            out += sql[i];
            i++;
          }
        }
        continue;
      }

      // 普通块注释 —— 移除
      i += 2;
      while (i < len && !(sql[i] === "*" && sql[i + 1] === "/")) i++;
      i += 2;
      if (out && !out.endsWith(" ")) out += " ";
      continue;
    }

    // 行注释 -- ...
    if (ch === "-" && next === "-") {
      i += 2;
      while (i < len && sql[i] !== "\n") i++;
      continue;
    }

    // PostgreSQL dollar-quoted 字符串：$$...$$ 或 $tag$...$tag$
    if (dialect === "postgres" && ch === "$") {
      const tagMatch = DOLLAR_QUOTE_RE.exec(sql.slice(i));
      if (tagMatch && tagMatch.index === 0) {
        const tag = tagMatch[0];
        out += tag;
        i += tag.length;
        // 找到结束 tag（原样保留内部所有内容，包括换行与 --）
        while (i < len) {
          if (sql.startsWith(tag, i)) {
            out += tag;
            i += tag.length;
            break;
          }
          out += sql[i];
          i++;
        }
        continue;
      }
    }

    // 单引号字符串字面量（处理 '' 转义；MySQL 额外处理反斜杠转义）
    if (ch === "'") {
      out += "'";
      i++;
      while (i < len) {
        const c = sql[i];
        // MySQL 反斜杠转义：\' \\ \n 等 —— 原样保留反斜杠与下一字符
        if (dialect === "mysql" && c === "\\" && i + 1 < len) {
          out += c;
          out += sql[i + 1];
          i += 2;
          continue;
        }
        out += c;
        if (c === "'") {
          if (sql[i + 1] === "'") {
            out += sql[i + 1];
            i += 2;
            continue;
          }
          i++;
          break;
        }
        i++;
      }
      continue;
    }

    // 双引号标识符（处理 "" 转义）
    if (ch === '"') {
      out += '"';
      i++;
      while (i < len) {
        out += sql[i];
        if (sql[i] === '"') {
          if (sql[i + 1] === '"') {
            out += sql[i + 1];
            i += 2;
            continue;
          }
          i++;
          break;
        }
        i++;
      }
      continue;
    }

    // 反引号标识符（MySQL）
    if (ch === "`") {
      out += "`";
      i++;
      while (i < len && sql[i] !== "`") {
        out += sql[i];
        i++;
      }
      if (i < len) {
        out += "`";
        i++;
      }
      continue;
    }

    // SQL Server 方括号标识符 [...]（]] 为转义 ]）
    if (dialect === "sqlserver" && ch === "[") {
      out += "[";
      i++;
      while (i < len) {
        const c = sql[i];
        out += c;
        if (c === "]") {
          if (sql[i + 1] === "]") {
            out += sql[i + 1];
            i += 2;
            continue;
          }
          i++;
          break;
        }
        i++;
      }
      continue;
    }

    // 空白 —— 折叠为单个空格
    if (isWhitespace(ch)) {
      collapseWhitespace();
      continue;
    }

    out += ch;
    i++;
  }

  return out.trim();
}

/**
 * Format SQL for *display* (object source, view/table DDL viewers).
 *
 * Unlike `formatSqlText`, this never throws: if the SQL can't be parsed by the
 * formatter (vendor-specific DDL, oversized input, ...) the original text is
 * returned unchanged so the viewer still shows the source. Use this for
 * read-only/auto-format surfaces; use `formatSqlText` where a thrown error
 * should surface to the user (e.g. the explicit "Format SQL" command).
 */
export async function formatSqlForDisplay(sql: string, dialect: SqlFormatDialect = "generic", settings: Partial<SqlFormatterSettings> = DEFAULT_SQL_FORMATTER_SETTINGS): Promise<string> {
  if (!sql.trim()) return sql;
  try {
    return await formatSqlText(sql, dialect, settings);
  } catch {
    return sql;
  }
}
