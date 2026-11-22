import { localizeProjectErrors } from "../i18n/diagnostics";

export type TypedResult<T, E> =
  | { status: "ok"; data: T }
  | { status: "error"; error: E };

export async function unwrap<T>(
  result: Promise<TypedResult<T, string>>,
): Promise<T> {
  const resolved = await result;
  if (resolved.status === "error") {
    throw new Error(resolved.error);
  }
  return resolved.data;
}

export function messageOf(error: unknown): string {
  const message = error instanceof Error ? error.message : String(error);
  // "el proyecto tiene errores: [E_…] error: …" from the engine, in the
  // interface language and naming each problem.
  return localizeProjectErrors(message) ?? message;
}
