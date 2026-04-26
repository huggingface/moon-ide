// Cheap, non-crypto hash for content fingerprints. We use it to detect
// "is this file still equal to what's on disk" without keeping a second
// full copy of the text in memory.
//
// FNV-1a 32-bit. Two arithmetic ops per char, no allocation. Collision
// probability is ~1 in 4×10⁹ for arbitrary inputs; we additionally store
// the byte length so two strings of different lengths can never compare
// equal even if their hashes did collide.

export type ContentFingerprint = {
	length: number;
	hash: number;
};

export function fingerprint(text: string): ContentFingerprint {
	let hash = 0x811c9dc5;
	for (let i = 0; i < text.length; i++) {
		hash ^= text.charCodeAt(i);
		hash = Math.imul(hash, 0x01000193);
	}
	return { length: text.length, hash: hash >>> 0 };
}

export function fingerprintEquals(a: ContentFingerprint, b: ContentFingerprint): boolean {
	return a.length === b.length && a.hash === b.hash;
}
