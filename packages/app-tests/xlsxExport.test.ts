import { strict as assert } from "node:assert";
import { test } from "vitest";
import { buildXlsxWorkbook, buildXlsxWorkbookMulti } from "../../apps/desktop/src/lib/export/xlsxExport.ts";
import { buildXlsxSqlWorksheet } from "../../apps/desktop/src/lib/export/xlsxSqlSheet.ts";

test("builds an xlsx workbook zip with worksheet data", () => {
  const workbook = buildXlsxWorkbook({
    sheetName: "Users",
    columns: ["id", "name", "active"],
    rows: [
      [1, "Ada & Bob", true],
      [2, null, false],
    ],
  });
  const text = new TextDecoder().decode(workbook);

  assert.equal(workbook[0], 0x50);
  assert.equal(workbook[1], 0x4b);
  assert.match(text, /\[Content_Types\]\.xml/);
  assert.match(text, /xl\/worksheets\/sheet1\.xml/);
  assert.match(text, /name="Users"/);
  assert.match(text, /<c r="A2"><v>1<\/v><\/c>/);
  assert.match(text, /Ada &amp; Bob/);
  assert.match(text, /<c r="C2" t="b"><v>1<\/v><\/c>/);
});

test("sanitizes invalid sheet names", () => {
  const workbook = buildXlsxWorkbook({
    sheetName: "bad/name:with*chars?and-a-very-long-tail",
    columns: ["value"],
    rows: [["ok"]],
  });
  const text = new TextDecoder().decode(workbook);

  assert.match(text, /name="bad name with chars and-a-very-"/);
});

test("writes MySQL 5.7 numeric strings as numeric cells", () => {
  const workbook = buildXlsxWorkbook({
    sheetName: "MySQL 5.7",
    columns: ["nullable_int", "float_value", "double_value", "decimal_value", "bigint_high_precision"],
    columnTypes: ["int(11)", "float", "double", "decimal(18,6)", "bigint(20)"],
    rows: [["42", "123.5", "987654.321", "2800.000000", "9007199254740992"]],
  });
  const text = new TextDecoder().decode(workbook);

  // Numeric columns get the right-align cellXfs style (index 2).
  assert.match(text, /<c r="A2" s="2"><v>42<\/v><\/c>/);
  assert.match(text, /<c r="B2" s="2"><v>123\.5<\/v><\/c>/);
  assert.match(text, /<c r="C2" s="2"><v>987654\.321<\/v><\/c>/);
  assert.match(text, /<c r="D2" s="2"><v>2800\.000000<\/v><\/c>/);
  assert.match(text, /<c r="E2" t="inlineStr" s="2"><is><t>9007199254740992<\/t><\/is><\/c>/);
});

test("numeric columns get right-align style in xlsx exports", () => {
  const workbook = buildXlsxWorkbook({
    sheetName: "Mix",
    columns: ["amount", "label"],
    columnTypes: ["decimal(10,2)", "varchar(50)"],
    rows: [[1.5, "row"]],
  });
  const text = new TextDecoder().decode(workbook);

  // styles.xml defines a third cellXfs entry with right alignment for numeric columns.
  assert.match(text, /<cellXfs count="3">/);
  assert.match(text, /<alignment horizontal="right"\/>/);
  // Numeric column (decimal) carries s="2"; text column (varchar) has no style attribute.
  assert.match(text, /<c r="A2" s="2"><v>1\.5<\/v><\/c>/);
  assert.match(text, /<c r="B2" t="inlineStr"><is><t>row<\/t><\/is><\/c>/);
});

test("xlsx numeric detection matches the data grid (bit is not numeric, serial/dec/fixed are)", () => {
  // bit/bool are boolean flags in the grid and must NOT get the right-align
  // style; serial/dec/fixed ARE numeric in the grid and must get it. This keeps
  // exports aligned with the in-app grid.
  const workbook = buildXlsxWorkbook({
    sheetName: "Sync",
    columns: ["flag", "seq", "amt", "price"],
    columnTypes: ["bit", "serial", "dec(10,2)", "fixed"],
    rows: [[1, 100, 9.99, 3.14]],
  });
  const text = new TextDecoder().decode(workbook);

  // bit -> no right-align style (boolean flag, left-aligned like the grid)
  assert.match(text, /<c r="A2"><v>1<\/v><\/c>/);
  // serial / dec / fixed -> right-align style (numeric, matches grid)
  assert.match(text, /<c r="B2" s="2"><v>100<\/v><\/c>/);
  assert.match(text, /<c r="C2" s="2"><v>9\.99<\/v><\/c>/);
  assert.match(text, /<c r="D2" s="2"><v>3\.14<\/v><\/c>/);
});

test("builds a result workbook with a separate SQL worksheet", () => {
  const sqlWorksheet = buildXlsxSqlWorksheet([{ sql: "SELECT id, name FROM users WHERE active = true" }]);
  assert.ok(sqlWorksheet);
  const workbook = buildXlsxWorkbookMulti([{ sheetName: "Result", columns: ["id", "name"], rows: [[1, "Ada"]] }, sqlWorksheet]);
  const text = new TextDecoder().decode(workbook);

  assert.match(text, /name="Result"/);
  assert.match(text, /name="SQL"/);
  assert.match(text, /xl\/worksheets\/sheet2\.xml/);
  assert.match(text, /SELECT id, name FROM users WHERE active = true/);
});

test("maps multiple result statements and splits SQL at the Excel cell limit", () => {
  const bmpPrefix = "x".repeat(32_766);
  const longSql = `${bmpPrefix}😀tail`;
  const worksheet = buildXlsxSqlWorksheet([
    { resultName: "Result 1", sql: "SELECT 1" },
    { resultName: "Result 2", sql: longSql },
  ]);

  assert.ok(worksheet);
  assert.deepEqual(worksheet.columns, ["Result", "SQL"]);
  assert.equal(worksheet.rows.length, 3);
  assert.deepEqual(worksheet.rows[0], ["Result 1", "SELECT 1"]);
  const longSqlRows = worksheet.rows.slice(1);
  assert.ok(longSqlRows.every((row) => String(row[1]).length <= 32_767));
  assert.equal(longSqlRows[0][1], bmpPrefix);
  assert.equal(longSqlRows[1][1], "😀tail");
  assert.equal(longSqlRows.map((row) => row[1]).join(""), longSql);
});
