import { label } from './format.js';
export function render(list, items) {
  for (const item of items) {
    const li = document.createElement('li');
    li.textContent = label(item);
    list.appendChild(li);
  }
}
