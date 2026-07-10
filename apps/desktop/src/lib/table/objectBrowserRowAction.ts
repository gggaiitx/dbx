import type { ObjectBrowserRow } from "@/lib/table/objectBrowserRows";

export type ObjectBrowserRowAction = "table-info" | "open-table" | "open-source" | "none";

/**
 * Determine the action for a single click on an object browser row.
 * - TABLE → table-info (show table properties panel)
 * - VIEW/MATERIALIZED_VIEW/PROCEDURE/FUNCTION/SEQUENCE/PACKAGE/PACKAGE_BODY → open-source
 * - otherwise → none
 */
export function singleClickRowAction(row: ObjectBrowserRow | null | undefined): ObjectBrowserRowAction {
  if (!row) return "none";
  if (row.type === "TABLE") return "table-info";
  if (canOpenSource(row)) return "open-source";
  return "none";
}

/**
 * Determine the action for a double click on an object browser row.
 * - TABLE → open-table (open table data tab)
 * - VIEW/MATERIALIZED_VIEW/PROCEDURE/FUNCTION/SEQUENCE/PACKAGE/PACKAGE_BODY → open-source
 * - otherwise → none
 */
export function doubleClickRowAction(row: ObjectBrowserRow | null | undefined): ObjectBrowserRowAction {
  if (!row) return "none";
  if (row.type === "TABLE") return "open-table";
  if (canOpenSource(row)) return "open-source";
  return "none";
}

/**
 * Resolve a row click event into a single or double action based on click detail
 * and the sidebar activation setting.
 */
export function resolveRowClickAction(row: ObjectBrowserRow | null | undefined, detail: number, activation: "single" | "double"): { action: ObjectBrowserRowAction; isDouble: boolean } {
  if (activation === "double") {
    if (detail === 2) return { action: doubleClickRowAction(row), isDouble: true };
    return { action: "none", isDouble: false };
  }
  // single-click activation
  if (detail > 1) return { action: doubleClickRowAction(row), isDouble: true };
  return { action: singleClickRowAction(row), isDouble: false };
}

/**
 * Whether a single-click action should be deferred to distinguish it from a
 * possible upcoming double-click. Only applies in single-click activation mode
 * and when the row's single-click and double-click actions differ (e.g. TABLE:
 * single → table-info, double → open-table). For rows whose single and double
 * actions are identical (e.g. VIEW → open-source both), no deferral is needed.
 */
export function shouldDeferSingleClick(row: ObjectBrowserRow | null | undefined, action: ObjectBrowserRowAction, activation: "single" | "double"): boolean {
  if (activation !== "single") return false;
  if (action === "none") return false;
  const single = singleClickRowAction(row);
  const double = doubleClickRowAction(row);
  return single !== double && action === single;
}

function canOpenSource(row: ObjectBrowserRow): boolean {
  return row.type === "VIEW" || row.type === "MATERIALIZED_VIEW" || row.type === "PROCEDURE" || row.type === "FUNCTION" || row.type === "SEQUENCE" || row.type === "PACKAGE" || row.type === "PACKAGE_BODY";
}
