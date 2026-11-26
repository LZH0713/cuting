import ReactDOM from 'react-dom/client';
import { detectWindow } from './lib/ipc';
import { Overlay } from './overlay/Overlay';
import { ResultWindow } from './result/ResultWindow';
import { ScrollDriver } from './scroll/ScrollDriver';
import { ScrollOutline } from './scroll/ScrollOutline';
import { ScrollPanel } from './scroll/ScrollPanel';
import { SettingsWindow } from './settings/SettingsWindow';
import './styles.css';

const target = detectWindow();
const root = ReactDOM.createRoot(document.getElementById('root')!);

if (target.kind === 'overlay') {
  root.render(<Overlay monitorId={target.monitorId} />);
} else if (target.kind === 'result') {
  root.render(<ResultWindow />);
} else if (target.kind === 'scroll-panel') {
  root.render(<ScrollPanel />);
} else if (target.kind === 'scroll-outline') {
  root.render(<ScrollOutline />);
} else if (target.kind === 'scroll-driver') {
  root.render(<ScrollDriver />);
} else {
  root.render(<SettingsWindow />);
}
