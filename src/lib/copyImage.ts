// Copy an `<img>`'s pixels to the system clipboard as an image.
// WebKitGTK's own "Copy Image" puts a `data:` image's URL on the
// clipboard as text, which no app pastes as a picture.

import { Image } from '@tauri-apps/api/image';
import { writeImage } from '@tauri-apps/plugin-clipboard-manager';

/** Decode `src` (data:, asset or blob URL) and write its RGBA pixels
 *  to the clipboard. Fetching and decoding the bytes, rather than
 *  drawing the element, keeps cross-origin sources from tainting the
 *  canvas. */
export async function copyImageToClipboard(src: string): Promise<void> {
	const blob = await (await fetch(src)).blob();
	const bitmap = await createImageBitmap(blob);
	try {
		const canvas = new OffscreenCanvas(bitmap.width, bitmap.height);
		const ctx = canvas.getContext('2d');
		if (ctx === null) {
			throw new Error('no 2d canvas context');
		}
		ctx.drawImage(bitmap, 0, 0);
		const { data } = ctx.getImageData(0, 0, bitmap.width, bitmap.height);
		const image = await Image.new(new Uint8Array(data.buffer), bitmap.width, bitmap.height);
		await writeImage(image);
	} finally {
		bitmap.close();
	}
}
