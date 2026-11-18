import { describe, expect, it } from 'vitest';
import { isPrivateAsset, repoOf } from './githubAssets';

describe('private GitHub attachments', () => {
	it('are the two attachment address shapes', () => {
		expect(isPrivateAsset('https://github.com/user-attachments/assets/368a38b4-7a05-4c27-8c4b-c7b5f9349a7f')).toBe(
			true,
		);
		expect(isPrivateAsset('https://github.com/acme/app/assets/15983122/0aa16e6e-8ce2-4b7b-88b8-f591ca5a4560')).toBe(
			true,
		);
	});

	it('are not ordinary images, signed ones or repository files', () => {
		expect(isPrivateAsset('https://user-images.githubusercontent.com/1/2-abc.png')).toBe(false);
		expect(isPrivateAsset('https://private-user-images.githubusercontent.com/1/2-368a38b4.png?jwt=x')).toBe(false);
		expect(isPrivateAsset('https://github.com/acme/app/blob/main/logo.png')).toBe(false);
		expect(isPrivateAsset('https://github.com/acme/app/assets/someone/0aa16e6e-8ce2')).toBe(false);
		expect(isPrivateAsset('https://example.test/user-attachments/assets/368a38b4-7a05')).toBe(false);
	});

	it('reads the repository off its page URL', () => {
		expect(repoOf('https://github.com/acme/app')).toBe('acme/app');
		expect(repoOf('https://github.com/acme/app/pull/3')).toBeNull();
		expect(repoOf(null)).toBeNull();
	});
});
