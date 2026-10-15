import { render } from 'list';
import { items } from './data.js';
render(document.getElementById('items'), items);
const status = document.createElement('p');
status.id = 'status';
status.textContent = 'modules ran';
document.body.appendChild(status);
