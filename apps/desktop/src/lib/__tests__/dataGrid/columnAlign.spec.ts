import { describe, expect, it } from "vitest";
import { isNumericColumnType } from "@/lib/dataGrid/dataGridColumnType";

describe("isNumericColumnType", () => {
  it("recognizes core numeric types", () => {
    expect(isNumericColumnType("int")).toBe(true);
    expect(isNumericColumnType("integer")).toBe(true);
    expect(isNumericColumnType("bigint")).toBe(true);
    expect(isNumericColumnType("smallint")).toBe(true);
    expect(isNumericColumnType("decimal")).toBe(true);
    expect(isNumericColumnType("numeric")).toBe(true);
    expect(isNumericColumnType("number")).toBe(true);
    expect(isNumericColumnType("float")).toBe(true);
    expect(isNumericColumnType("double")).toBe(true);
    expect(isNumericColumnType("real")).toBe(true);
    expect(isNumericColumnType("money")).toBe(true);
    expect(isNumericColumnType("smallmoney")).toBe(true);
  });

  it("recognizes types with precision/scale suffix", () => {
    expect(isNumericColumnType("decimal(10,2)")).toBe(true);
    expect(isNumericColumnType("numeric(18,6)")).toBe(true);
    expect(isNumericColumnType("int(11)")).toBe(true);
    expect(isNumericColumnType("bigint(20)")).toBe(true);
    expect(isNumericColumnType("float(53)")).toBe(true);
  });

  it("recognizes serial, int aliases, and Oracle types", () => {
    expect(isNumericColumnType("serial")).toBe(true);
    expect(isNumericColumnType("bigserial")).toBe(true);
    expect(isNumericColumnType("int2")).toBe(true);
    expect(isNumericColumnType("int4")).toBe(true);
    expect(isNumericColumnType("int8")).toBe(true);
    expect(isNumericColumnType("binary_float")).toBe(true);
    expect(isNumericColumnType("binary_double")).toBe(true);
  });

  it("recognizes unsigned and ClickHouse types", () => {
    expect(isNumericColumnType("uint8")).toBe(true);
    expect(isNumericColumnType("uint64")).toBe(true);
    expect(isNumericColumnType("float32")).toBe(true);
    expect(isNumericColumnType("float64")).toBe(true);
  });

  it("rejects non-numeric types", () => {
    expect(isNumericColumnType("varchar")).toBe(false);
    expect(isNumericColumnType("text")).toBe(false);
    expect(isNumericColumnType("date")).toBe(false);
    expect(isNumericColumnType("timestamp")).toBe(false);
    expect(isNumericColumnType("boolean")).toBe(false);
    expect(isNumericColumnType("blob")).toBe(false);
    expect(isNumericColumnType("json")).toBe(false);
    expect(isNumericColumnType("uuid")).toBe(false);
  });

  it("handles edge cases", () => {
    expect(isNumericColumnType(undefined)).toBe(false);
    expect(isNumericColumnType("")).toBe(false);
    expect(isNumericColumnType("DECIMAL")).toBe(true); // case-insensitive
    expect(isNumericColumnType("  decimal(10,2)  ")).toBe(true); // whitespace tolerant
  });

  it("recognizes dec and fixed aliases", () => {
    expect(isNumericColumnType("dec")).toBe(true);
    expect(isNumericColumnType("fixed")).toBe(true);
  });
});
