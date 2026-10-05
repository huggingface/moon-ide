import { beforeEach, describe, expect, it, vi } from 'vitest';

import type { ContainerStatus, ProjectComposeStatus } from './protocol';

const ipcMock = vi.hoisted(() => ({
	container: {
		status: vi.fn(),
		rebuild: vi.fn(),
		setup: vi.fn(),
	},
	projectCompose: {
		status: vi.fn(),
		rebuild: vi.fn(),
		up: vi.fn(),
	},
}));

vi.mock('./ipc', () => ({ ipc: ipcMock }));

const { container } = await import('./container.svelte');
const { projectCompose } = await import('./projectCompose.svelte');

const FOLDER = '/repo';
const containerStatus: ContainerStatus = { state: 'running', services: [] };
const projectStatus: ProjectComposeStatus = {
	folder_path: FOLDER,
	compose_file: `${FOLDER}/compose.yaml`,
	project_name: 'repo',
	status: containerStatus,
};

beforeEach(() => {
	vi.resetAllMocks();
	container.lastError = null;
	projectCompose.forget(FOLDER);
});

describe('workspace container action errors', () => {
	it('survive the post-failure refresh and later background refreshes', async () => {
		ipcMock.container.rebuild.mockRejectedValue(new Error('pull access denied for moon-base'));
		ipcMock.container.status.mockResolvedValue(containerStatus);

		await container.rebuild();
		expect(ipcMock.container.status).toHaveBeenCalled();
		expect(container.lastError).toBe('pull access denied for moon-base');

		await container.refresh();
		expect(container.lastError).toBe('pull access denied for moon-base');
	});

	it('clear when the next action starts', async () => {
		ipcMock.container.rebuild.mockRejectedValue(new Error('boom'));
		ipcMock.container.status.mockResolvedValue(containerStatus);
		await container.rebuild();

		ipcMock.container.setup.mockResolvedValue(containerStatus);
		await container.setup();
		expect(container.lastError).toBeNull();
	});

	it('refresh errors still clear on the next good refresh', async () => {
		ipcMock.container.setup.mockResolvedValue(containerStatus);
		await container.setup();

		ipcMock.container.status.mockRejectedValueOnce(new Error('daemon gone'));
		await container.refresh();
		expect(container.lastError).toBe('daemon gone');

		ipcMock.container.status.mockResolvedValue(containerStatus);
		await container.refresh();
		expect(container.lastError).toBeNull();
	});
});

describe('project compose action errors', () => {
	it('survive the post-failure refresh and polling refreshes', async () => {
		ipcMock.projectCompose.rebuild.mockRejectedValue(new Error('pull failed'));
		ipcMock.projectCompose.status.mockResolvedValue(projectStatus);

		await projectCompose.rebuild(FOLDER);
		expect(ipcMock.projectCompose.status).toHaveBeenCalledWith(FOLDER);
		expect(projectCompose.errorFor(FOLDER)).toBe('pull failed');

		await projectCompose.refresh(FOLDER);
		expect(projectCompose.errorFor(FOLDER)).toBe('pull failed');
	});

	it('clear when the next action on that folder starts', async () => {
		ipcMock.projectCompose.rebuild.mockRejectedValue(new Error('pull failed'));
		ipcMock.projectCompose.status.mockResolvedValue(projectStatus);
		await projectCompose.rebuild(FOLDER);

		ipcMock.projectCompose.up.mockResolvedValue(projectStatus);
		await projectCompose.up(FOLDER);
		expect(projectCompose.errorFor(FOLDER)).toBeUndefined();
	});

	it('refresh errors still clear on the next good refresh', async () => {
		ipcMock.projectCompose.status.mockRejectedValueOnce(new Error('daemon gone'));
		await projectCompose.refresh(FOLDER);
		expect(projectCompose.errorFor(FOLDER)).toBe('daemon gone');

		ipcMock.projectCompose.status.mockResolvedValue(projectStatus);
		await projectCompose.refresh(FOLDER);
		expect(projectCompose.errorFor(FOLDER)).toBeUndefined();
	});
});
