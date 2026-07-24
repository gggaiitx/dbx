import { describe, expect, it } from "vitest";
import { isNumericColumnType } from "@/lib/dataGrid/dataGridColumnType";

describe("isNumericColumnType", () => {
  it("returns true for integer family types", () => {
    expect(isNumericColumnType("INTEGER")).toBe(true);
    expect(isNumericColumnType("bigint")).toBe(true);
    expect(isNumericColumnType("SMALLINT")).toBe(true);
    expect(isNumericColumnType("tinyint")).toBe(true);
    expect(isNumericColumnType("mediumint")).toBe(true);
    expect(isNumericColumnType("int(11)")).toBe(true);
    expect(isNumericColumnType("int2")).toBe(true);
    expect(isNumericColumnType("int4")).toBe(true);
    expect(isNumericColumnType("int8")).toBe(true);
    expect(isNumericColumnType("uint64")).toBe(true);
    expect(isNumericColumnType("serial")).toBe(true);
    expect(isNumericColumnType("bigserial")).toBe(true);
  });

  it("returns true for decimal/float family types", () => {
    expect(isNumericColumnType("DECIMAL")).toBe(true);
    expect(isNumericColumnType("decimal(10,2)")).toBe(true);
    expect(isNumericColumnType("NUMERIC")).toBe(true);
    expect(isNumericColumnType("NUMBER")).toBe(true);
    expect(isNumericColumnType("FLOAT")).toBe(true);
    expect(isNumericColumnType("float8")).toBe(true);
    expect(isNumericColumnType("DOUBLE")).toBe(true);
    expect(isNumericColumnType("real")).toBe(true);
    expect(isNumericColumnType("MONEY")).toBe(true);
    expect(isNumericColumnType("smallmoney")).toBe(true);
    expect(isNumericColumnType("dec")).toBe(true);
    expect(isNumericColumnType("fixed")).toBe(true);
  });

  it("returns false for non-numeric types", () => {
    expect(isNumericColumnType("VARCHAR(255)")).toBe(false);
    expect(isNumericColumnType("text")).toBe(false);
    expect(isNumericColumnType("DATE")).toBe(false);
    expect(isNumericColumnType("timestamp")).toBe(false);
    expect(isNumericColumnType("BOOLEAN")).toBe(false);
    expect(isNumericColumnType("bytea")).toBe(false);
    expect(isNumericColumnType("json")).toBe(false);
  });

  it("treats bit/bool as non-numeric (boolean flags, not aligned)", () => {
    expect(isNumericColumnType("bit")).toBe(false);
    expect(isNumericColumnType("bit(1)")).toBe(false);
    expect(isNumericColumnType("bool")).toBe(false);
    expect(isNumericColumnType("boolean")).toBe(false);
  });

  it("normalizes case, spaces, and type modifiers", () => {
    expect(isNumericColumnType("  Integer  ")).toBe(true);
    expect(isNumericColumnType("INT UNSIGNED")).toBe(true);
    expect(isNumericColumnType("decimal(10,5)")).toBe(true);
    expect(isNumericColumnType("NUMBER(38)")).toBe(true);
  });

  it("returns false for undefined / empty so callers fall back to left-align", () => {
    expect(isNumericColumnType(undefined)).toBe(false);
    expect(isNumericColumnType("")).toBe(false);
    expect(isNumericColumnType("   ")).toBe(false);
  });
});
