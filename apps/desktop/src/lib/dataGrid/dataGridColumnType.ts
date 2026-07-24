/**
 * Resolve the data type to display in a data-grid column header.
 *
 * Two sources can supply a column's type:
 *  - Table metadata (only when a table is open): matched **by column name**,
 *    richer because it carries precision/scale. Preferred.
 *  - `QueryResult.column_types` (any query): parallel to `result.columns`, so
 *    it must be read **by index**. Used as a fallback for arbitrary queries
 *    that have no table metadata (e.g. `select * from pg_depend`).
 *
 * Returns `undefined` when neither source has a non-empty type, so callers can
 * simply hide the type row.
 */
export interface HeaderColumnTypeSources {
  /** Type from table metadata for this column (looked up by name), if any. */
  tableColumnType?: string;
  /** `QueryResult.column_types`, parallel to `result.columns` (by index). */
  resultColumnTypes?: readonly string[];
  /** Index of the column within `result.columns`. */
  actualColIdx: number;
}

export function resolveHeaderColumnType({ tableColumnType, resultColumnTypes, actualColIdx }: HeaderColumnTypeSources): string | undefined {
  const fromMeta = tableColumnType?.trim();
  if (fromMeta) return fromMeta;

  const fromResult = resultColumnTypes?.[actualColIdx]?.trim();
  return fromResult ? fromResult : undefined;
}

export function compactHeaderColumnType(dataType: string): string {
  return /^enum\s*\(/i.test(dataType.trim()) ? "enum" : dataType;
}

const NUMERIC_COLUMN_TYPE_BASES = new Set([
  "tinyint",
  "smallint",
  "mediumint",
  "int",
  "integer",
  "bigint",
  "serial",
  "smallserial",
  "bigserial",
  "int2",
  "int4",
  "int8",
  "uint",
  "uint8",
  "uint16",
  "uint32",
  "uint64",
  "uint128",
  "uint256",
  "float",
  "float4",
  "float8",
  "float32",
  "float64",
  "real",
  "double",
  "decimal",
  "numeric",
  "number",
  "dec",
  "fixed",
  "money",
  "smallmoney",
]);

/**
 * 是否为数值类型列（用于右对齐判断）。
 *
 * 归一化策略：trim + toLowerCase，取首个空白/左括号/左方括号前的 base 名，
 * 与现有 isNumericColumnType 检测一致。`bit`/`bool` 不算数值（通常为布尔标志）。
 * 传入 undefined / 空串时返回 false，调用方按左对齐回退。
 */
export function isNumericColumnType(dataType: string | undefined): boolean {
  if (!dataType) return false;
  const base = dataType
    .trim()
    .toLowerCase()
    .split(/[\s([]/, 1)[0];
  return NUMERIC_COLUMN_TYPE_BASES.has(base);
}
