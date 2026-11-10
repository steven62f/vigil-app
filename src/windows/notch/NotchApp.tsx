import { useEffect, useState } from "react";
import { useAgentStore } from "../../stores/agentStore";
import { useTauriEvents } from "../../hooks/useTauriEvents";
import { NotchContainer, MIN_W } from "../../components/notch/NotchContainer";
import { ListPanel } from "../../components/list/ListPanel";
import { DetailPanel } from "../../components/detail/DetailPanel";
import { SettingsProvider } from "../../hooks/SettingsProvider";
import { NavigationProvider, useNavigation } from "../../hooks/NavigationProvider";
import { useAutoHighlight, isWaitingState } from "../../hooks/useAutoHighlight";

function NotchViews() {
  const agents = useAgentStore((s) => s.agents);
  const selectedTranscriptPath = useAgentStore((s) => s.selectedTranscriptPath);
  const { view, detailFromList, autoOpenedForWaiting, setView, closeDetail } = useNavigation();
  const [notchWidth, setNotchWidth] = useState(MIN_W);

  useAutoHighlight();

  const selectedAgent = agents.find((a) => a.transcriptPath === selectedTranscriptPath) ?? null;

  // Si l'agent disparaît du store pendant la vue détail, revenir au notch
  useEffect(() => {
    if (view === "detail" && !selectedAgent) closeDetail();
  }, [view, selectedAgent, closeDetail]);

  // Repli auto : un détail auto-ouvert POUR une demande (permission/question) a été affiché SANS
  // focus (le highlight ne vole pas le focus). Si la demande se résout alors que Vigil n'a jamais
  // reçu le focus — donc réponse au terminal ou via Telegram — aucun `blur` ne fermera le panneau :
  // on le replie nous-mêmes. Si l'utilisateur a répondu DANS Vigil (fenêtre focus), on ne touche à
  // rien, le `blur` existant s'en charge quand il cliquera ailleurs.
  useEffect(() => {
    if (view !== "detail" || !autoOpenedForWaiting || !selectedAgent) return;
    if (isWaitingState(selectedAgent.state)) return;
    if (!document.hasFocus()) closeDetail();
  }, [view, autoOpenedForWaiting, selectedAgent, closeDetail]);

  return (
    <>
      {view === "notch" && (
        <NotchContainer
          agents={agents}
          onOpenList={() => setView("list")}
          onWidthChange={setNotchWidth}
        />
      )}
      {view === "list" && (
        <ListPanel
          agents={agents}
          onClose={() => setView("notch")}
          notchWidth={notchWidth}
        />
      )}
      {view === "detail" && selectedAgent && (
        <DetailPanel
          agent={selectedAgent}
          fromList={detailFromList}
          onBack={() => setView(detailFromList ? "list" : "notch")}
          onClose={closeDetail}
        />
      )}
    </>
  );
}

export function NotchApp() {
  useTauriEvents();

  return (
    <SettingsProvider>
      <NavigationProvider>
        <NotchViews />
      </NavigationProvider>
    </SettingsProvider>
  );
}
