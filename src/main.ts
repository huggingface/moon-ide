import { mount } from 'svelte';
import App from './App.svelte';
import './styles.css';

const target = document.getElementById('app');
if (!target) {
	throw new Error('mount target #app not found');
}

// Wipe the inline boot splash (`index.html`) before Svelte mounts so
// we don't end up with both the static markup and the component
// splash layered in the same container. Svelte 5's `mount()`
// appends; it doesn't replace.
target.replaceChildren();

mount(App, { target });
