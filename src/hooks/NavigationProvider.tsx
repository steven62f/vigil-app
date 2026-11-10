import { createContext, useContext, useState } from "react";
import { useAgentStore } from "../stores/agentStore";

export type View = "notch" | "list" | "detail";

interface NavigationContextValue {
  view: View;
  detailFromList: boolean;
  // Vrai UNIQUEMENT quand le détail a été auto-ouvert par le highlight POUR une demande
  // (waitingApprove/waitingSpecific). Sert au repli auto quand la demande se résout ailleurs
  // (terminal/Telegram) : la fenêtre n'ayant jamais reçu le focus, aucun `blur` ne la fermerait.
  autoOpenedForWaiting: boolean;
  setView: (view: View) => void;
  openDetail: (transcriptPath: string, fromList: boolean, autoOpenedForWaiting?: boolean) => void;
  closeDetail: () => void;
}

const NavigationContext = createContext<NavigationContextValue>(null!);

export function NavigationProvider({ children }: { children: React.ReactNode }) {
  const [view, setView] = useState<View>("notch");
  const [detailFromList, setDetailFromList] = useState(false);
  const [autoOpenedForWaiting, setAutoOpenedForWaiting] = useState(false);
  const selectAgent = useAgentStore((s) => s.selectAgent);

  const openDetail = (transcriptPath: string, fromList: boolean, autoWaiting = false) => {
    selectAgent(transcriptPath);
    setDetailFromList(fromList);
    setAutoOpenedForWaiting(autoWaiting);
    setView("detail");
  };

  const closeDetail = () => {
    selectAgent(null);
    setAutoOpenedForWaiting(false);
    setView("notch");
  };

  return (
    <NavigationContext.Provider
      value={{ view, detailFromList, autoOpenedForWaiting, setView, openDetail, closeDetail }}
    >
      {children}
    </NavigationContext.Provider>
  );
}

export function useNavigation(): NavigationContextValue {
  return useContext(NavigationContext);
}
