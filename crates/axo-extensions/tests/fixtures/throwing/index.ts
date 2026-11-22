// Test-only extension whose module evaluation throws: the bridge must disable only this
// member and keep its group siblings serving.
throw new Error("throwing fixture: import fails on purpose");
