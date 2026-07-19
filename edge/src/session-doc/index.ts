/**
 * Vendored subset of comet's `@comet/session-doc` — exactly what the edge
 * needs: the tunable constants, message-entry shapes + continuation stitching,
 * sidecar payload types, and the DO tail materializer. Byte-faithful to the
 * originals except: control-plane types are vendored (control-types.ts) and
 * the loro-mirror schema/commands modules are not carried (the DO never opens
 * a Mirror — `tail.ts` holds the two schema.ts functions it does use).
 */
export * from "./constants";
export * from "./control-types";
export * from "./render-parts";
export * from "./messages";
export * from "./sidecar";
export * from "./tail";
