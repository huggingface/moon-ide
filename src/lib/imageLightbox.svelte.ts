/** One app-wide full-size image viewer, shared by pasted attachments,
 *  tool-result images and images in transcript markdown. */
export interface LightboxImage {
	src: string;
	alt: string;
	/** Workspace path, when the image is a workspace file: the viewer
	 *  then offers "Open in editor". */
	workspacePath?: string | undefined;
}

export const lightbox = $state<{ image: LightboxImage | null }>({ image: null });

export function openLightbox(image: LightboxImage): void {
	lightbox.image = image;
}

export function closeLightbox(): void {
	lightbox.image = null;
}
