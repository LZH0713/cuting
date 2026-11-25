import ReactDOM from 'react-dom/client';
import { detectWindow } from './lib/ipc';
import { Overlay } from './overlay/Overlay';
import { ResultWindow } from './result/ResultWindow';
import { SettingsWindow } from './settings/SettingsWindow';
import './styles.css';

const target = detectWindow();
const root = ReactDOM.createRoot(document.getElementById('root')!);

if (target.kind === 'overlay') {
  root.render(<Overlay />);
} else if (target.kind === 'result') {
  root.render(<ResultWindow />);
} else {
  root.render(<SettingsWindow />);
}
